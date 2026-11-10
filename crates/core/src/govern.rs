//! Live regulation of a running operation: the counters of [`Stats`] in, the knobs of
//! [`Control`] out (ADR 0012).
//!
//! Every ~100 ms [`Governor::tick`] turns what the workers did since the previous tick into a
//! [`Sample`] for the [`Tuner`], and [`start`] puts the tuner's answer on the control, where
//! the workers read it at their next safe point. Nothing here touches the OS: what only the
//! OS can tell (outside pressure, user input, CPU time) comes in through a [`SignalsHook`]
//! the caller passes, the same way a priority goes out through the worker hook.
//!
//! The counters are live because every working thread hands its tally over every ~25 ms at
//! its safe points (see the stats module), not only when it ends.

use std::io;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::control::Control;
use crate::options::Options;
use crate::stats::{Stats, StatsSnapshot};
use crate::tune::{Decision, Knobs, Mode, Sample, TuneBounds, Tuner};

/// Time between two samples.
pub const TICK: Duration = Duration::from_millis(100);
/// A tick shorter than this (a spurious wake-up) is not a sample.
const MIN_TICK: Duration = Duration::from_millis(50);

/// What only the OS can tell, read once per tick.
#[derive(Debug, Clone, Copy, Default)]
pub struct Signals {
    /// Pressure from outside this operation, 0 to 1 (see [`Sample::pressure`]).
    pub pressure: Option<f64>,
    /// The user touched the machine since the previous tick.
    pub user_input: bool,
    /// User-mode CPU time of the whole process so far. Its growth between two ticks, set
    /// against the busy time of the workers, shows whether a thread waited for a core.
    pub process_user_cpu: Option<Duration>,
}

/// Called on the governor's thread once per tick.
pub type SignalsHook = Box<dyn FnMut() -> Signals + Send>;

/// Turns counters into samples and samples into decisions. No thread, no clock.
#[derive(Debug)]
pub struct Governor {
    tuner: Tuner,
    last: StatsSnapshot,
    last_cpu: Option<Duration>,
}

impl Governor {
    pub fn new(tuner: Tuner) -> Self {
        Governor {
            tuner,
            last: StatsSnapshot::default(),
            last_cpu: None,
        }
    }

    pub fn tuner(&self) -> &Tuner {
        &self.tuner
    }

    pub fn tuner_mut(&mut self) -> &mut Tuner {
        &mut self.tuner
    }

    pub fn into_tuner(self) -> Tuner {
        self.tuner
    }

    /// One tick: `elapsed` since the previous one, `now` the counters at its end.
    pub fn tick(&mut self, elapsed: Duration, now: &StatsSnapshot, signals: Signals) -> Decision {
        let cpu = match (signals.process_user_cpu, self.last_cpu) {
            (Some(now), Some(before)) => Some(now.saturating_sub(before)),
            _ => None,
        };
        self.last_cpu = signals.process_user_cpu;
        let sample = sample(elapsed, &self.last, now, cpu, signals);
        self.last = *now;
        self.tuner.step(&sample)
    }
}

/// The sample for the span between two snapshots.
fn sample(
    elapsed: Duration,
    last: &StatsSnapshot,
    now: &StatsSnapshot,
    cpu: Option<Duration>,
    signals: Signals,
) -> Sample {
    let time = |a: Duration, b: Duration| a.saturating_sub(b);
    let count = |a: u64, b: u64| a.saturating_sub(b);
    let busy = time(now.busy, last.busy);
    Sample {
        elapsed,
        bytes: count(now.read_bytes, last.read_bytes) + count(now.write_bytes, last.write_bytes),
        io_ops: count(now.io_ops, last.io_ops),
        io_wait: time(now.read, last.read) + time(now.write, last.write),
        busy,
        // Only meaningful next to some busy time.
        cpu: cpu.filter(|_| !busy.is_zero()),
        pressure: signals.pressure,
        user_input: signals.user_input,
    }
}

/// Puts the knobs on the control, where the workers read them at their next safe point.
pub fn apply(control: &Control, knobs: Knobs) {
    control.set_threads(knobs.workers);
    control.set_chunk(knobs.chunk);
    control.set_io_depth(knobs.io_depth);
}

enum Command {
    Mode(Mode),
    Bounds(TuneBounds),
    Stop,
}

/// A governor running on its own thread. Dropping it stops the thread; [`stop`](Self::stop)
/// also hands the tuner back, to read [`Tuner::best`].
pub struct Running {
    tx: Sender<Command>,
    handle: Option<JoinHandle<Tuner>>,
}

impl Running {
    /// Changes what the operation is for. Applies at the next tick.
    pub fn set_mode(&self, mode: Mode) {
        let _ = self.tx.send(Command::Mode(mode));
    }

    /// Changes the ceilings (profile switch, battery, heat). Lower ones apply at the next tick.
    pub fn set_bounds(&self, bounds: TuneBounds) {
        let _ = self.tx.send(Command::Bounds(bounds));
    }

    /// Stops the thread and returns the tuner, or `None` if the thread died.
    pub fn stop(mut self) -> Option<Tuner> {
        let _ = self.tx.send(Command::Stop);
        self.handle.take()?.join().ok()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = self.tx.send(Command::Stop);
            let _ = handle.join();
        }
    }
}

/// Starts regulating the operation `opts` describes. Returns `None` when stats are off,
/// because the governor has nothing to read.
///
/// The first knobs and the copy-buffer ceiling are put on the control before this returns,
/// so an operation started after it sees them. The tuner's bounds set the ceiling: they come
/// from the caller's memory budget.
pub fn start(
    opts: &Options,
    tuner: Tuner,
    signals: Option<SignalsHook>,
) -> io::Result<Option<Running>> {
    let Some(stats) = opts.stats.clone() else {
        return Ok(None);
    };
    let control = opts.control.clone();
    control.set_chunk_ceiling(tuner.bounds().max_chunk);
    apply(&control, tuner.knobs());
    let (tx, rx) = mpsc::channel();
    let handle = thread::Builder::new()
        .name("zstar-governor".into())
        .spawn(move || run(Governor::new(tuner), &stats, &control, signals, &rx))?;
    Ok(Some(Running {
        tx,
        handle: Some(handle),
    }))
}

fn run(
    mut governor: Governor,
    stats: &Stats,
    control: &Control,
    mut signals: Option<SignalsHook>,
    rx: &mpsc::Receiver<Command>,
) -> Tuner {
    let mut last = Instant::now();
    loop {
        let decision = match rx.recv_timeout(TICK.saturating_sub(last.elapsed())) {
            Ok(Command::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Command::Mode(mode)) => Some(governor.tuner_mut().set_mode(mode)),
            Ok(Command::Bounds(bounds)) => {
                control.set_chunk_ceiling(bounds.max_chunk);
                Some(governor.tuner_mut().set_bounds(bounds))
            }
            Err(RecvTimeoutError::Timeout) => None,
        };
        if let Some(d) = decision.filter(|d| d.changed) {
            apply(control, d.knobs);
        }
        let elapsed = last.elapsed();
        if elapsed < MIN_TICK {
            continue;
        }
        last = Instant::now();
        let signals = signals.as_mut().map(|f| f()).unwrap_or_default();
        let snapshot = stats.snapshot(control.parked());
        let d = governor.tick(elapsed, &snapshot, signals);
        if d.changed {
            apply(control, d.knobs);
        }
    }
    governor.into_tuner()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds() -> TuneBounds {
        TuneBounds {
            max_workers: 4,
            max_io_depth: 16,
            min_chunk: 64 << 10,
            max_chunk: 1 << 20,
        }
    }

    fn snap(ms: u64, bytes: u64, ops: u64, busy_ms: u64) -> StatsSnapshot {
        StatsSnapshot {
            read: Duration::from_millis(ms),
            read_bytes: bytes,
            io_ops: ops,
            busy: Duration::from_millis(busy_ms),
            ..StatsSnapshot::default()
        }
    }

    #[test]
    fn a_sample_is_the_difference_between_two_snapshots() {
        let signals = Signals {
            pressure: Some(0.2),
            user_input: true,
            process_user_cpu: None,
        };
        let s = sample(
            TICK,
            &snap(10, 1000, 4, 20),
            &snap(40, 5000, 14, 90),
            Some(Duration::from_millis(60)),
            signals,
        );
        assert_eq!(s.elapsed, TICK);
        assert_eq!((s.bytes, s.io_ops), (4000, 10));
        assert_eq!(s.io_wait, Duration::from_millis(30));
        assert_eq!(s.busy, Duration::from_millis(70));
        assert_eq!(s.cpu, Some(Duration::from_millis(60)));
        assert_eq!((s.pressure, s.user_input), (Some(0.2), true));
    }

    #[test]
    fn cpu_time_means_nothing_without_busy_time_and_counters_never_go_backwards() {
        let s = sample(
            TICK,
            &snap(40, 5000, 14, 90),
            &snap(40, 5000, 14, 90),
            Some(Duration::from_millis(5)),
            Signals::default(),
        );
        assert_eq!((s.cpu, s.bytes, s.io_ops), (None, 0, 0));
        let s = sample(
            TICK,
            &snap(40, 5000, 14, 90),
            &snap(10, 1000, 4, 20),
            None,
            Signals::default(),
        );
        assert_eq!((s.bytes, s.busy), (0, Duration::ZERO));
    }

    #[test]
    fn process_cpu_is_turned_into_a_delta_from_the_second_tick_on() {
        let mut g = Governor::new(Tuner::new(Mode::Foreground, bounds(), None));
        let at = |ms| Signals {
            process_user_cpu: Some(Duration::from_millis(ms)),
            ..Signals::default()
        };
        g.tick(TICK, &snap(0, 0, 0, 0), at(500));
        assert_eq!(g.last_cpu, Some(Duration::from_millis(500)));
        g.tick(TICK, &snap(0, 0, 0, 0), Signals::default());
        assert_eq!(g.last_cpu, None);
    }

    #[test]
    fn the_first_knobs_reach_the_control_before_start_returns() {
        let stats = Stats::new();
        let opts = Options {
            stats: Some(stats),
            ..Options::default()
        };
        let tuner = Tuner::new(Mode::Foreground, bounds(), None);
        let first = tuner.knobs();
        let running = start(&opts, tuner, None).unwrap().unwrap();
        assert_eq!(opts.control.chunk_ceiling(), bounds().max_chunk);
        assert_eq!(opts.control.threads(), first.workers);
        assert_eq!(opts.control.chunk(), first.chunk);
        assert_eq!(opts.control.io_depth(), first.io_depth);
        assert!(running.stop().is_some());
    }

    #[test]
    fn without_stats_there_is_nothing_to_govern() {
        let tuner = Tuner::new(Mode::Foreground, bounds(), None);
        assert!(start(&Options::default(), tuner, None).unwrap().is_none());
    }

    #[test]
    fn commands_reach_the_tuner_and_the_control() {
        let opts = Options {
            stats: Some(Stats::new()),
            ..Options::default()
        };
        let running = start(&opts, Tuner::new(Mode::Foreground, bounds(), None), None)
            .unwrap()
            .unwrap();
        let lower = TuneBounds {
            max_workers: 1,
            max_io_depth: 2,
            min_chunk: 64 << 10,
            max_chunk: 256 << 10,
        };
        running.set_bounds(lower);
        running.set_mode(Mode::BackgroundActive);
        let tuner = running.stop().unwrap();
        assert_eq!(tuner.mode(), Mode::BackgroundActive);
        assert_eq!(tuner.bounds(), lower);
        assert_eq!(opts.control.chunk_ceiling(), 256 << 10);
        assert!(opts.control.threads() <= 1);
        assert!(opts.control.chunk() <= 256 << 10);
    }
}
