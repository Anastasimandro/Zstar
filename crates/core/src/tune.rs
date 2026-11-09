//! Adaptive tuning by measurement (ADR 0012).
//!
//! [`Tuner`] decides how hard an operation presses on the CPU and on the drive, from what it
//! measures while it works. It never asks what kind of drive it is on: an NVMe, a HDD, an SD
//! card or a network share are told apart only by how they answer.
//!
//! It is pure. The caller feeds it one [`Sample`] per tick (about 100 ms) and applies the
//! [`Knobs`] it gets back at the next safe point. No clock, no thread and no OS call live
//! here, so every decision can be replayed in tests against a simulated machine.
//!
//! Three regimes, chosen by the caller through [`Mode`]:
//!
//! * `Foreground`: the user is waiting. Maximise throughput within the ceilings.
//! * `BackgroundActive`: the user is doing something else. Use only what is left: halve at
//!   once when the operation slows other work down, come back one step at a time.
//! * `BackgroundAway`: nobody is at the machine. Maximise throughput, but yield and switch to
//!   `BackgroundActive` on the first user input.
//!
//! **Throughput.** One knob at a time. A change is kept when the median throughput of the
//! next window beats the previous one by more than `GAIN`; a step down is kept when it costs
//! less than `HOLD_TOL`, because the same speed with less pressure is better. A fresh start
//! doubles each knob while it pays (slow start); later probes move one step.
//!
//! **Interference**, noticed without OS calls: at a low priority, competition shows up in the
//! operation's own numbers. Average I/O latency above `LAT_LIMIT` times the best seen with
//! the same queue depth and request size means someone else is using the drive. Bytes per
//! busy second below `EFF_LIMIT` times the best seen with the same worker count means
//! someone else is using the cores, or the machine is throttling. [`Sample::pressure`] is an
//! optional third signal from `crates/sys`. References expire after `BASE_TTL` ticks so
//! they follow the machine.
//!
//! The ceilings come from the caller (profile and memory budget); the tuner never goes above
//! them and never below one worker and one request, so an operation always finishes.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::time::Duration;

/// Smallest gain that makes a step up worth keeping.
const GAIN: f64 = 0.05;
/// Largest loss a step down may cost and still be kept.
const HOLD_TOL: f64 = 0.02;
/// Ticks ignored after a change, while the new setting settles.
const SETTLE_TICKS: u32 = 1;
/// Ticks measured per window; the median of them is the window's throughput.
const MEASURE_TICKS: usize = 8;
/// Windows between probes in the foreground and when nobody is at the machine.
const PROBE_EVERY_FG: u32 = 10;
/// Windows between probes while yielding to the user: slow recovery.
const PROBE_EVERY_BG: u32 = 2;
/// Change in throughput at the same setting that means the machine changed: explore again.
const RESTART_DRIFT: f64 = 0.25;
/// I/O latency over the reference that counts as someone else using the drive.
const LAT_LIMIT: f64 = 1.5;
/// Bytes per busy second under the reference that counts as someone else using the cores.
const EFF_LIMIT: f64 = 0.85;
/// Outside pressure (0 to 1) that counts as interference.
const PRESSURE_LIMIT: f64 = 0.10;
/// Contended ticks in a row before yielding, so one noisy tick does not count.
const BRAKE_TICKS: u32 = 2;
/// Ticks without any probe after yielding or falling back.
const COOLDOWN_TICKS: u32 = 20;
/// Ticks after which a latency or efficiency reference is replaced by a fresh measurement.
const BASE_TTL: u64 = 600;
/// Invalid samples in a row before falling back to safe fixed values.
const FALLBACK_TICKS: u32 = 20;
/// Request size with no memory: safe on every drive.
const DEFAULT_CHUNK: usize = 1 << 20;
/// Smallest request size the tuner accepts as a bound.
const MIN_CHUNK_FLOOR: usize = 4096;

/// Every knob, in the order a fresh start explores them.
const ALL_KNOBS: [Knob; 3] = [Knob::IoDepth, Knob::Workers, Knob::Chunk];
/// What later probes try, in turn: each knob up, then each knob down.
const ROTATION: [(Knob, bool); 6] = [
    (Knob::IoDepth, true),
    (Knob::Workers, true),
    (Knob::Chunk, true),
    (Knob::IoDepth, false),
    (Knob::Workers, false),
    (Knob::Chunk, false),
];

/// The situation the operation runs in; it decides what the tuner optimises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// The user is waiting for the result: maximum throughput.
    #[default]
    Foreground,
    /// Running in the background while the user works: use only what is left.
    BackgroundActive,
    /// Running in the background with nobody at the machine: maximum throughput until the
    /// first user input.
    BackgroundAway,
}

/// One of the values the tuner moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Knob {
    /// Compression or decompression workers.
    Workers,
    /// Drive requests in flight: files in parallel, or read-ahead.
    IoDepth,
    /// Size of each drive request.
    Chunk,
}

/// Why the tuner yielded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// I/O latency rose without any change of ours.
    Disk,
    /// Work per busy second fell without any change of ours.
    Cpu,
    /// The OS reported pressure this operation did not cause.
    Pressure,
    /// The user touched the machine while it was away.
    UserInput,
}

/// What the last step did, for `--stats` and logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Nothing to change.
    Hold,
    /// Trying a new value for this knob.
    Explore(Knob),
    /// The new value paid off and stays.
    Keep(Knob),
    /// The new value did not pay off; the previous one is back.
    Revert(Knob),
    /// Pressure was halved to make room for other work.
    Yield(Cause),
    /// The mode or the ceilings changed.
    ModeChange,
    /// The measurements made no sense; safe fixed values are in place.
    Fallback,
}

/// The values the tuner decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Knobs {
    /// Compression or decompression workers, at least 1.
    pub workers: usize,
    /// Drive requests in flight, at least 1.
    pub io_depth: usize,
    /// Bytes per drive request.
    pub chunk: usize,
}

/// Ceilings the tuner stays within. They come from the profile and the memory budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TuneBounds {
    /// Most workers; in [`Mode::BackgroundActive`] one or two are left free. Pass physical
    /// cores, not logical ones, so a background worker does not share a core with the user.
    pub max_workers: usize,
    /// Most drive requests in flight.
    pub max_io_depth: usize,
    /// Smallest request size.
    pub min_chunk: usize,
    /// Largest request size.
    pub max_chunk: usize,
}

impl TuneBounds {
    /// The same bounds with nothing at zero and nothing upside down.
    fn sane(self) -> Self {
        let min_chunk = self.min_chunk.max(MIN_CHUNK_FLOOR);
        TuneBounds {
            max_workers: self.max_workers.max(1),
            max_io_depth: self.max_io_depth.max(1),
            min_chunk,
            max_chunk: self.max_chunk.max(min_chunk),
        }
    }
}

/// What the operation did during one tick, as deltas of the stats counters.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Sample {
    /// Wall time the tick covers.
    pub elapsed: Duration,
    /// Bytes produced during the tick.
    pub bytes: u64,
    /// Drive reads and writes completed.
    pub io_ops: u64,
    /// Time spent inside those reads and writes, summed over threads.
    pub io_wait: Duration,
    /// Time workers spent working, summed over workers.
    pub busy: Duration,
    /// Pressure on the machine this operation did not cause, 0.0 to 1.0, if `crates/sys`
    /// can tell. Linux CPU and I/O PSI include this operation's own threads and must not be
    /// passed raw; memory PSI can be.
    pub pressure: Option<f64>,
    /// The user touched keyboard, mouse or screen during the tick, if `crates/sys` can tell.
    pub user_input: bool,
}

/// The knobs to apply after a step and why they are what they are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    /// Values to apply at the next safe point.
    pub knobs: Knobs,
    /// Whether they differ from the previous step.
    pub changed: bool,
    /// Why.
    pub reason: Reason,
}

#[derive(Debug, Clone, Copy)]
struct Trial {
    knob: Knob,
    up: bool,
    from: Knobs,
    /// Throughput measured at `from`.
    base: f64,
    /// Part of the slow start rather than a later probe.
    slow: bool,
}

#[derive(Debug, Clone, Copy)]
struct Explore {
    knob: Knob,
    up: bool,
    /// At least one step in this direction paid off.
    progressed: bool,
}

#[derive(Debug, Clone, Copy)]
struct Base {
    value: f64,
    at: u64,
}

/// The adaptive tuner of one operation. See the module documentation.
#[derive(Debug, Clone)]
pub struct Tuner {
    mode: Mode,
    bounds: TuneBounds,
    knobs: Knobs,
    tick: u64,
    since_change: u32,
    window: Vec<f64>,
    /// Throughput of the current knobs, once measured.
    reference: Option<f64>,
    trial: Option<Trial>,
    /// Knobs still to explore with slow start.
    queue: VecDeque<Explore>,
    /// Steps double instead of adding one.
    exploring: bool,
    epochs_since_probe: u32,
    rotation: usize,
    cooldown: u32,
    contended: u32,
    invalid: u32,
    /// Lowest latency per request, by queue depth and request size.
    lat_base: HashMap<(usize, usize), Base>,
    /// Highest bytes per busy second, by worker count.
    eff_base: HashMap<usize, Base>,
    best: Option<(f64, Knobs)>,
}

impl Tuner {
    /// A tuner for a new operation. `remembered` is what [`Tuner::best`] returned last time on
    /// the same volume; without it the tuner starts from safe values and explores.
    ///
    /// Operations shorter than about a second finish before the tuner learns anything; for
    /// those, use `remembered` or the defaults and skip the tuner.
    pub fn new(mode: Mode, bounds: TuneBounds, remembered: Option<Knobs>) -> Self {
        let bounds = bounds.sane();
        let mut tuner = Tuner {
            mode,
            bounds,
            knobs: Knobs {
                workers: 1,
                io_depth: 1,
                chunk: bounds.min_chunk,
            },
            tick: 0,
            since_change: 0,
            window: Vec::with_capacity(MEASURE_TICKS),
            reference: None,
            trial: None,
            queue: VecDeque::new(),
            exploring: false,
            epochs_since_probe: 0,
            rotation: 0,
            cooldown: 0,
            contended: 0,
            invalid: 0,
            lat_base: HashMap::new(),
            eff_base: HashMap::new(),
            best: None,
        };
        match remembered {
            Some(knobs) => tuner.knobs = tuner.clamp(knobs),
            None => {
                let workers = if mode == Mode::BackgroundActive {
                    2
                } else {
                    tuner.worker_cap()
                };
                tuner.knobs = tuner.clamp(Knobs {
                    workers,
                    io_depth: 2,
                    chunk: DEFAULT_CHUNK,
                });
                tuner.queue = explore(&ALL_KNOBS);
                tuner.exploring = true;
            }
        }
        tuner
    }

    /// The values to apply now.
    pub fn knobs(&self) -> Knobs {
        self.knobs
    }

    /// The current mode. It changes by itself from `BackgroundAway` to `BackgroundActive` on
    /// user input.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The fastest knobs measured outside [`Mode::BackgroundActive`], to remember per volume.
    pub fn best(&self) -> Option<Knobs> {
        self.best.map(|(_, knobs)| knobs)
    }

    /// Switches mode. Lower ceilings apply at once; leaving `BackgroundActive` explores upwards
    /// again.
    pub fn set_mode(&mut self, mode: Mode) -> Decision {
        let old = self.knobs;
        let was = self.mode;
        self.mode = mode;
        self.knobs = self.clamp(self.knobs);
        self.trial = None;
        self.window.clear();
        self.reference = None;
        self.contended = 0;
        if was == Mode::BackgroundActive && mode != Mode::BackgroundActive {
            self.queue = explore(&[Knob::IoDepth, Knob::Workers]);
            self.exploring = true;
            self.cooldown = 0;
        }
        self.decide(old, Reason::ModeChange)
    }

    /// Replaces the ceilings, for example when the profile changes, the laptop goes on battery
    /// or the machine runs hot. Lower ceilings apply at once.
    pub fn set_bounds(&mut self, bounds: TuneBounds) -> Decision {
        let old = self.knobs;
        self.bounds = bounds.sane();
        self.knobs = self.clamp(self.knobs);
        // A pending revert could restore values above the new ceilings.
        self.trial = None;
        self.window.clear();
        self.reference = None;
        self.decide(old, Reason::ModeChange)
    }

    /// Takes one tick of measurements and returns the knobs to apply.
    pub fn step(&mut self, sample: &Sample) -> Decision {
        self.tick += 1;
        let old = self.knobs;
        let secs = sample.elapsed.as_secs_f64();
        if secs <= 0.0 {
            self.invalid += 1;
            if self.invalid >= FALLBACK_TICKS {
                self.invalid = 0;
                return self.fallback();
            }
            return self.hold();
        }
        self.invalid = 0;
        if sample.bytes == 0 && sample.io_ops == 0 {
            // Idle, for example between files: nothing to learn.
            return self.hold();
        }
        if sample.user_input && self.mode == Mode::BackgroundAway {
            self.mode = Mode::BackgroundActive;
            return self.yield_to(Cause::UserInput);
        }
        self.since_change = self.since_change.saturating_add(1);
        if self.since_change <= SETTLE_TICKS {
            return self.hold();
        }
        if self.mode == Mode::BackgroundActive {
            match self.interference(sample) {
                Some(cause) => {
                    self.contended += 1;
                    if self.contended >= BRAKE_TICKS {
                        return self.yield_to(cause);
                    }
                    // A contended tick says nothing about our own speed.
                    return self.hold();
                }
                None => self.contended = 0,
            }
        }
        self.learn(sample);
        self.cooldown = self.cooldown.saturating_sub(1);
        self.window.push(sample.bytes as f64 / secs);
        if self.window.len() < MEASURE_TICKS {
            return self.hold();
        }
        let rate = median(&mut self.window);
        self.window.clear();
        let reason = self.on_epoch(rate);
        self.decide(old, reason)
    }

    fn worker_cap(&self) -> usize {
        let max = self.bounds.max_workers;
        match self.mode {
            Mode::BackgroundActive => {
                let spare = if max > 4 { 2 } else { 1 };
                max.saturating_sub(spare).max(1)
            }
            Mode::Foreground | Mode::BackgroundAway => max,
        }
    }

    fn clamp(&self, knobs: Knobs) -> Knobs {
        Knobs {
            workers: knobs.workers.clamp(1, self.worker_cap()),
            io_depth: knobs.io_depth.clamp(1, self.bounds.max_io_depth),
            chunk: knobs
                .chunk
                .clamp(self.bounds.min_chunk, self.bounds.max_chunk),
        }
    }

    fn hold(&self) -> Decision {
        Decision {
            knobs: self.knobs,
            changed: false,
            reason: Reason::Hold,
        }
    }

    fn decide(&mut self, old: Knobs, reason: Reason) -> Decision {
        let changed = old != self.knobs;
        if changed {
            self.since_change = 0;
            self.window.clear();
            self.contended = 0;
        }
        Decision {
            knobs: self.knobs,
            changed,
            reason,
        }
    }

    fn interference(&self, sample: &Sample) -> Option<Cause> {
        if sample.pressure.is_some_and(|p| p > PRESSURE_LIMIT) {
            return Some(Cause::Pressure);
        }
        if sample.io_ops > 0 {
            let latency = sample.io_wait.as_secs_f64() / sample.io_ops as f64;
            let key = (self.knobs.io_depth, self.knobs.chunk);
            if fresh(&self.lat_base, &key, self.tick).is_some_and(|b| latency > b * LAT_LIMIT) {
                return Some(Cause::Disk);
            }
        }
        let busy = sample.busy.as_secs_f64();
        if busy > 0.0 {
            let efficiency = sample.bytes as f64 / busy;
            let reference = fresh(&self.eff_base, &self.knobs.workers, self.tick);
            if reference.is_some_and(|b| efficiency < b * EFF_LIMIT) {
                return Some(Cause::Cpu);
            }
        }
        None
    }

    fn learn(&mut self, sample: &Sample) {
        if sample.io_ops > 0 {
            let latency = sample.io_wait.as_secs_f64() / sample.io_ops as f64;
            let key = (self.knobs.io_depth, self.knobs.chunk);
            update(&mut self.lat_base, key, latency, self.tick, |new, old| {
                new < old
            });
        }
        let busy = sample.busy.as_secs_f64();
        if busy > 0.0 {
            let efficiency = sample.bytes as f64 / busy;
            let key = self.knobs.workers;
            update(
                &mut self.eff_base,
                key,
                efficiency,
                self.tick,
                |new, old| new > old,
            );
        }
    }

    fn yield_to(&mut self, cause: Cause) -> Decision {
        let old = self.knobs;
        self.knobs = self.clamp(Knobs {
            workers: (self.knobs.workers / 2).max(1),
            io_depth: (self.knobs.io_depth / 2).max(1),
            chunk: self.knobs.chunk,
        });
        self.trial = None;
        self.queue.clear();
        self.exploring = false;
        self.reference = None;
        self.contended = 0;
        self.cooldown = COOLDOWN_TICKS;
        self.epochs_since_probe = 0;
        self.window.clear();
        self.decide(old, Reason::Yield(cause))
    }

    fn fallback(&mut self) -> Decision {
        let old = self.knobs;
        self.knobs = self.clamp(Knobs {
            workers: 2,
            io_depth: 2,
            chunk: DEFAULT_CHUNK,
        });
        self.trial = None;
        self.queue.clear();
        self.exploring = false;
        self.window.clear();
        self.reference = None;
        self.cooldown = COOLDOWN_TICKS;
        self.decide(old, Reason::Fallback)
    }

    fn on_epoch(&mut self, rate: f64) -> Reason {
        if self.mode != Mode::BackgroundActive && self.best.is_none_or(|(best, _)| rate > best) {
            self.best = Some((rate, self.knobs));
        }
        if let Some(trial) = self.trial.take() {
            return self.finish_trial(trial, rate);
        }
        let drifted = self
            .reference
            .is_some_and(|r| (rate - r).abs() > r * RESTART_DRIFT);
        if drifted
            && self.queue.is_empty()
            && self.cooldown == 0
            && self.mode != Mode::BackgroundActive
        {
            // Same knobs, very different speed: the machine changed under us.
            self.queue = explore(&ALL_KNOBS);
            self.exploring = true;
        }
        self.reference = Some(rate);
        self.next_trial(rate).unwrap_or(Reason::Hold)
    }

    fn finish_trial(&mut self, trial: Trial, rate: f64) -> Reason {
        let kept = if trial.up {
            rate > trial.base * (1.0 + GAIN)
        } else {
            rate >= trial.base * (1.0 - HOLD_TOL)
        };
        let reason = if kept {
            self.reference = Some(rate);
            Reason::Keep(trial.knob)
        } else {
            self.knobs = trial.from;
            self.reference = Some(trial.base);
            Reason::Revert(trial.knob)
        };
        if trial.slow && self.queue.front().is_some_and(|f| f.knob == trial.knob) {
            let pop = match self.queue.front_mut() {
                Some(front) if kept => {
                    front.progressed = true;
                    false
                }
                Some(front) if trial.up && !front.progressed => {
                    front.up = false;
                    false
                }
                _ => true,
            };
            if pop {
                self.queue.pop_front();
            }
        }
        let base = self.reference.unwrap_or(rate);
        self.next_trial(base).unwrap_or(reason)
    }

    fn next_trial(&mut self, base: f64) -> Option<Reason> {
        if self.cooldown > 0 {
            return None;
        }
        loop {
            let (knob, up, progressed, slow) = if let Some(front) = self.queue.front().copied() {
                self.exploring = true;
                (front.knob, front.up, front.progressed, true)
            } else {
                self.epochs_since_probe += 1;
                let every = if self.mode == Mode::BackgroundActive {
                    PROBE_EVERY_BG
                } else {
                    PROBE_EVERY_FG
                };
                if self.epochs_since_probe < every {
                    return None;
                }
                self.epochs_since_probe = 0;
                self.exploring = false;
                let (knob, up) = ROTATION[self.rotation % ROTATION.len()];
                self.rotation = self.rotation.wrapping_add(1);
                (knob, up, false, false)
            };
            let next = self.moved(knob, up);
            if next != self.knobs {
                self.trial = Some(Trial {
                    knob,
                    up,
                    from: self.knobs,
                    base,
                    slow,
                });
                self.knobs = next;
                return Some(Reason::Explore(knob));
            }
            if !slow {
                return None;
            }
            // This knob cannot move that way: try the other way once, or move on.
            if up && !progressed {
                if let Some(front) = self.queue.front_mut() {
                    front.up = false;
                }
            } else {
                self.queue.pop_front();
            }
        }
    }

    fn moved(&self, knob: Knob, up: bool) -> Knobs {
        let mut knobs = self.knobs;
        match knob {
            Knob::Workers => knobs.workers = step_count(knobs.workers, up, self.exploring),
            Knob::IoDepth => knobs.io_depth = step_count(knobs.io_depth, up, self.exploring),
            Knob::Chunk => {
                knobs.chunk = if up {
                    knobs.chunk.saturating_mul(2)
                } else {
                    knobs.chunk / 2
                }
            }
        }
        self.clamp(knobs)
    }
}

fn explore(knobs: &[Knob]) -> VecDeque<Explore> {
    knobs
        .iter()
        .map(|&knob| Explore {
            knob,
            up: true,
            progressed: false,
        })
        .collect()
}

fn step_count(value: usize, up: bool, double: bool) -> usize {
    match (up, double) {
        (true, true) => value.saturating_mul(2),
        (true, false) => value.saturating_add(1),
        (false, true) => value / 2,
        (false, false) => value.saturating_sub(1),
    }
}

fn median(window: &mut [f64]) -> f64 {
    window.sort_by(f64::total_cmp);
    window[window.len() / 2]
}

fn fresh<K: Eq + Hash>(map: &HashMap<K, Base>, key: &K, tick: u64) -> Option<f64> {
    map.get(key)
        .filter(|b| tick.saturating_sub(b.at) <= BASE_TTL)
        .map(|b| b.value)
}

fn update<K: Eq + Hash>(
    map: &mut HashMap<K, Base>,
    key: K,
    value: f64,
    tick: u64,
    better: fn(f64, f64) -> bool,
) {
    let keep = map
        .get(&key)
        .is_some_and(|b| tick.saturating_sub(b.at) <= BASE_TTL && !better(value, b.value));
    if !keep {
        map.insert(key, Base { value, at: tick });
    }
}

#[cfg(test)]
mod tests {
    //! A simulated machine: a drive with bandwidth, latency, useful parallelism and a seek
    //! penalty, cores with a fixed speed, and an optional competitor on the drive or the
    //! cores. The same model was used to tune the constants above.

    use super::*;

    const MIB: f64 = 1_048_576.0;
    const DT: f64 = 0.1;
    const COMP_CHUNK: usize = 1 << 20;

    #[derive(Debug, Clone, Copy)]
    struct Machine {
        bw: f64,
        lat: f64,
        par: usize,
        thrash: f64,
        cores: usize,
        per_core: f64,
    }

    fn machine(bw: f64, lat: f64, par: usize, thrash: f64, cores: usize, core: f64) -> Machine {
        Machine {
            bw: bw * MIB,
            lat,
            par,
            thrash,
            cores,
            per_core: core * MIB,
        }
    }

    fn machines() -> Vec<(&'static str, Machine)> {
        vec![
            ("nvme", machine(3000.0, 0.000_08, 32, 0.0, 8, 150.0)),
            ("sata_ssd", machine(500.0, 0.000_1, 8, 0.0, 8, 150.0)),
            ("hdd", machine(150.0, 0.008, 1, 0.25, 8, 150.0)),
            ("sd", machine(20.0, 0.002, 2, 0.1, 4, 60.0)),
            ("usb2", machine(35.0, 0.001, 1, 0.15, 4, 60.0)),
            ("net", machine(110.0, 0.004, 16, 0.0, 8, 150.0)),
            ("slow_cpu", machine(3000.0, 0.000_08, 32, 0.0, 4, 30.0)),
        ]
    }

    fn named(name: &str) -> Machine {
        machines()
            .into_iter()
            .find(|(n, _)| *n == name)
            .map(|(_, m)| m)
            .unwrap()
    }

    fn bounds() -> TuneBounds {
        TuneBounds {
            max_workers: 8,
            max_io_depth: 64,
            min_chunk: 128 << 10,
            max_chunk: 4 << 20,
        }
    }

    /// Our I/O rate, latency per request and the competitor's rate, with `d` requests of ours
    /// and `q` of a competitor in flight, all of `c` bytes.
    fn disk(m: &Machine, d: usize, c: usize, q: usize) -> (f64, f64, f64) {
        let n = (d + q) as f64;
        let c = c as f64;
        let t = m.lat + c / m.bw;
        let mut total = m.bw.min(n.min(m.par as f64) * c / t);
        if d + q > m.par {
            total *= (1.0 - m.thrash * (d + q - m.par) as f64).max(0.3);
        }
        (total * d as f64 / n, n * c / total, total * q as f64 / n)
    }

    /// Compression speed of `w` workers while a competitor keeps `comp` cores busy at a
    /// higher priority; a worker on a busy core gets a tenth of it.
    fn cpu(m: &Machine, w: usize, comp: usize) -> f64 {
        let free = m.cores.saturating_sub(comp);
        let full = w.min(free);
        let shared = w.min(m.cores) - full;
        m.per_core * (full as f64 + 0.1 * shared as f64)
    }

    /// (rate, latency per request, CPU capacity, competitor rate).
    fn rate_of(m: &Machine, k: Knobs, q: usize, comp: usize) -> (f64, f64, f64, f64) {
        let (io, lat, comp_rate) = disk(m, k.io_depth, k.chunk, q);
        let cap = cpu(m, k.workers, comp);
        (io.min(cap), lat, cap, comp_rate)
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        /// A factor between 0.97 and 1.03.
        fn noise(&mut self) -> f64 {
            0.97 + 0.06 * ((self.next() >> 11) as f64 / (1u64 << 53) as f64)
        }
    }

    /// One tick on the machine with the given knobs: (sample, our rate, competitor rate).
    fn sample(
        m: &Machine,
        k: Knobs,
        q: usize,
        comp: usize,
        rng: &mut Rng,
        user_input: bool,
    ) -> (Sample, f64, f64) {
        let (rate, lat, cap, comp_rate) = rate_of(m, k, q, comp);
        let rate = rate * rng.noise();
        let bytes = (rate * DT) as u64;
        let io_ops = (bytes / k.chunk as u64).max(1);
        let sample = Sample {
            elapsed: Duration::from_secs_f64(DT),
            bytes,
            io_ops,
            io_wait: Duration::from_secs_f64(io_ops as f64 * lat),
            busy: Duration::from_secs_f64(k.workers as f64 * DT * (rate / cap).min(1.0)),
            pressure: None,
            user_input,
        };
        (sample, rate, comp_rate)
    }

    /// The best throughput any fixed setting reaches on the machine.
    fn best_fixed(m: &Machine, b: TuneBounds) -> f64 {
        let mut best: f64 = 0.0;
        for workers in 1..=b.max_workers {
            let mut io_depth = 1;
            while io_depth <= b.max_io_depth {
                let mut chunk = b.min_chunk;
                while chunk <= b.max_chunk {
                    best = best.max(
                        rate_of(
                            m,
                            Knobs {
                                workers,
                                io_depth,
                                chunk,
                            },
                            0,
                            0,
                        )
                        .0,
                    );
                    chunk *= 2;
                }
                io_depth *= 2;
            }
        }
        best
    }

    fn run(m: &Machine, mode: Mode, ticks: usize, seed: u64) -> (Tuner, Vec<f64>) {
        let mut tuner = Tuner::new(mode, bounds(), None);
        let mut rng = Rng(seed);
        let mut rates = Vec::with_capacity(ticks);
        for _ in 0..ticks {
            let (s, rate, _) = sample(m, tuner.knobs(), 0, 0, &mut rng, false);
            tuner.step(&s);
            rates.push(rate);
        }
        (tuner, rates)
    }

    fn med(values: &[f64]) -> f64 {
        let mut v = values.to_vec();
        median(&mut v)
    }

    #[test]
    fn foreground_reaches_ninety_percent_of_the_best_fixed_setting_on_every_drive() {
        for (name, m) in machines() {
            let best = best_fixed(&m, bounds());
            for seed in [1, 7, 99, 12345] {
                let (tuner, rates) = run(&m, Mode::Foreground, 600, seed);
                let got = med(&rates[550..]);
                assert!(
                    got >= 0.9 * best,
                    "{name} seed {seed}: {got} of {best}, {tuner:?}"
                );
            }
        }
    }

    #[test]
    fn foreground_converges_within_fifteen_seconds() {
        for (name, m) in machines() {
            let best = best_fixed(&m, bounds());
            for seed in [1, 7, 99, 12345] {
                let (_, rates) = run(&m, Mode::Foreground, 150, seed);
                let got = med(&rates[100..150]);
                assert!(got >= 0.9 * best, "{name} seed {seed}: {got} of {best}");
            }
        }
    }

    #[test]
    fn background_does_not_yield_without_a_competitor() {
        for (name, m) in machines() {
            for seed in [1, 7, 99] {
                let mut tuner = Tuner::new(Mode::BackgroundActive, bounds(), None);
                let mut rng = Rng(seed);
                for tick in 0..800 {
                    let (s, _, _) = sample(&m, tuner.knobs(), 0, 0, &mut rng, false);
                    let d = tuner.step(&s);
                    assert!(
                        !matches!(d.reason, Reason::Yield(_)),
                        "{name} seed {seed} tick {tick}: {d:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn background_yields_to_a_disk_competitor_and_recovers() {
        for name in ["sata_ssd", "nvme"] {
            let m = named(name);
            let mut tuner = Tuner::new(Mode::BackgroundActive, bounds(), None);
            let mut rng = Rng(3);
            let (mut ours, mut theirs, mut knobs) = (Vec::new(), Vec::new(), Vec::new());
            for tick in 0..800 {
                let q = if (200..400).contains(&tick) { 8 } else { 0 };
                let (s, rate, comp_rate) = sample(&m, tuner.knobs(), q, 0, &mut rng, false);
                tuner.step(&s);
                ours.push(rate);
                if (220..400).contains(&tick) {
                    theirs.push(comp_rate);
                }
                knobs.push(tuner.knobs());
            }
            let pressure = |k: Knobs| k.io_depth * k.workers;
            assert!(
                pressure(knobs[215]) < pressure(knobs[199]),
                "{name}: {knobs:?}"
            );
            let alone = disk(&m, 0, COMP_CHUNK, 8).2;
            assert!(
                med(&theirs) >= 0.8 * alone,
                "{name}: competitor {}",
                med(&theirs) / alone
            );
            let (before, after) = (med(&ours[150..200]), med(&ours[750..]));
            assert!(
                after >= 0.7 * before,
                "{name}: {after} after vs {before} before"
            );
        }
    }

    #[test]
    fn background_yields_cores_to_a_cpu_competitor() {
        let m = named("nvme");
        let mut tuner = Tuner::new(Mode::BackgroundActive, bounds(), None);
        let mut rng = Rng(5);
        let mut knobs = Vec::new();
        for tick in 0..400 {
            let comp = if tick >= 200 { 6 } else { 0 };
            let (s, _, _) = sample(&m, tuner.knobs(), 0, comp, &mut rng, false);
            tuner.step(&s);
            knobs.push(tuner.knobs());
        }
        assert!(knobs[260].workers < knobs[199].workers, "{knobs:?}");
    }

    #[test]
    fn user_input_while_away_yields_at_once() {
        let m = named("nvme");
        let mut tuner = Tuner::new(Mode::BackgroundAway, bounds(), None);
        let mut rng = Rng(9);
        for tick in 0..100 {
            let (s, _, _) = sample(&m, tuner.knobs(), 0, 0, &mut rng, tick == 80);
            let d = tuner.step(&s);
            if tick == 80 {
                assert!(d.changed, "{d:?}");
                assert_eq!(d.reason, Reason::Yield(Cause::UserInput));
                assert_eq!(tuner.mode(), Mode::BackgroundActive);
            }
        }
    }

    #[test]
    fn user_input_in_the_foreground_changes_nothing() {
        let m = named("nvme");
        let mut tuner = Tuner::new(Mode::Foreground, bounds(), None);
        let mut rng = Rng(9);
        for tick in 0..100 {
            let (s, _, _) = sample(&m, tuner.knobs(), 0, 0, &mut rng, true);
            let d = tuner.step(&s);
            assert!(!matches!(d.reason, Reason::Yield(_)), "tick {tick}: {d:?}");
        }
        assert_eq!(tuner.mode(), Mode::Foreground);
    }

    #[test]
    fn knobs_stay_within_bounds_in_every_mode() {
        let b = bounds();
        for (name, m) in machines() {
            for mode in [
                Mode::Foreground,
                Mode::BackgroundActive,
                Mode::BackgroundAway,
            ] {
                let mut tuner = Tuner::new(mode, b, None);
                let mut rng = Rng(11);
                for tick in 0..1500 {
                    let q = (tick / 137) % 3 * 4;
                    let comp = (tick / 211) % 4 * 2;
                    let input = tick % 400 == 399;
                    let (s, _, _) = sample(&m, tuner.knobs(), q, comp, &mut rng, input);
                    let k = tuner.step(&s).knobs;
                    assert!(
                        (1..=b.max_workers).contains(&k.workers),
                        "{name} {mode:?}: {k:?}"
                    );
                    assert!(
                        (1..=b.max_io_depth).contains(&k.io_depth),
                        "{name} {mode:?}: {k:?}"
                    );
                    assert!(
                        (b.min_chunk..=b.max_chunk).contains(&k.chunk),
                        "{name}: {k:?}"
                    );
                    if tuner.mode() == Mode::BackgroundActive {
                        assert!(k.workers <= 6, "{name} {mode:?}: {k:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn remembered_knobs_are_the_starting_point() {
        let m = named("nvme");
        let start = Knobs {
            workers: 3,
            io_depth: 4,
            chunk: 2 << 20,
        };
        let mut tuner = Tuner::new(Mode::Foreground, bounds(), Some(start));
        let mut rng = Rng(2);
        for tick in 0..80 {
            let (s, _, _) = sample(&m, tuner.knobs(), 0, 0, &mut rng, false);
            assert!(!tuner.step(&s).changed, "tick {tick}");
        }
        assert_eq!(tuner.knobs(), start);
    }

    #[test]
    fn best_is_reported_for_the_next_run() {
        let (tuner, _) = run(&named("hdd"), Mode::Foreground, 300, 1);
        let best = tuner.best().unwrap();
        assert_eq!(best.io_depth, 1, "{best:?}");
    }

    #[test]
    fn invalid_samples_fall_back_to_safe_values() {
        let start = Knobs {
            workers: 8,
            io_depth: 32,
            chunk: 4 << 20,
        };
        let mut tuner = Tuner::new(Mode::Foreground, bounds(), Some(start));
        let empty = Sample::default();
        for _ in 1..FALLBACK_TICKS {
            assert_eq!(tuner.step(&empty).reason, Reason::Hold);
        }
        let d = tuner.step(&empty);
        assert_eq!(d.reason, Reason::Fallback);
        assert_eq!(
            d.knobs,
            Knobs {
                workers: 2,
                io_depth: 2,
                chunk: DEFAULT_CHUNK
            }
        );
    }

    #[test]
    fn idle_ticks_change_nothing() {
        let mut tuner = Tuner::new(Mode::Foreground, bounds(), None);
        let before = tuner.knobs();
        let idle = Sample {
            elapsed: Duration::from_millis(100),
            ..Sample::default()
        };
        for _ in 0..1000 {
            assert!(!tuner.step(&idle).changed);
        }
        assert_eq!(tuner.knobs(), before);
    }

    #[test]
    fn outside_pressure_makes_the_background_yield() {
        let m = named("nvme");
        let mut tuner = Tuner::new(Mode::BackgroundActive, bounds(), None);
        let mut rng = Rng(4);
        let mut yielded = false;
        for _ in 0..10 {
            let (mut s, _, _) = sample(&m, tuner.knobs(), 0, 0, &mut rng, false);
            s.pressure = Some(0.5);
            yielded |= tuner.step(&s).reason == Reason::Yield(Cause::Pressure);
        }
        assert!(yielded);
    }

    #[test]
    fn entering_the_background_lowers_the_worker_ceiling_at_once() {
        let mut tuner = Tuner::new(Mode::Foreground, bounds(), None);
        assert_eq!(tuner.knobs().workers, 8);
        let d = tuner.set_mode(Mode::BackgroundActive);
        assert!(d.changed);
        assert_eq!(d.reason, Reason::ModeChange);
        assert_eq!(d.knobs.workers, 6);
    }

    #[test]
    fn lower_ceilings_apply_at_once() {
        let mut tuner = Tuner::new(Mode::Foreground, bounds(), None);
        let low = TuneBounds {
            max_workers: 2,
            max_io_depth: 1,
            ..bounds()
        };
        let d = tuner.set_bounds(low);
        assert!(d.changed);
        assert_eq!((d.knobs.workers, d.knobs.io_depth), (2, 1));
    }

    #[test]
    fn senseless_bounds_are_made_safe() {
        let zero = TuneBounds {
            max_workers: 0,
            max_io_depth: 0,
            min_chunk: 0,
            max_chunk: 0,
        };
        let tuner = Tuner::new(Mode::BackgroundActive, zero, None);
        let k = tuner.knobs();
        assert_eq!((k.workers, k.io_depth, k.chunk), (1, 1, MIN_CHUNK_FLOOR));
    }
}
