//! Live control of a running operation.
//!
//! [`Control`] is the handle a caller keeps while an operation runs. Every value in it is an
//! atomic, so workers read it without locks. They only look at safe points (between files,
//! blocks or waves of blocks), never in the middle of a block, so a change takes effect at the
//! next boundary and cannot corrupt the output.
//!
//! Core makes no OS calls. Priority is only a value: the caller passes a [`WorkerHook`] and
//! every thread that works for the operation calls it when it first looks at the control and
//! whenever it sees the priority change. Turning that into a real priority is up to the hook,
//! and so is putting a thread back to normal when the operation is over.

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

use crate::error::Result;
use crate::profile::{Profile, cpu_rules, cpus};

/// How much a worker should yield to the rest of the system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Priority {
    /// The default priority.
    #[default]
    Normal,
    /// Below normal, so the operation does not slow the system down.
    Low,
}

/// Called on the thread that is about to work, with the priority it should run at.
pub type WorkerHook = Arc<dyn Fn(Priority) + Send + Sync>;

/// Credit the throughput limit lets build up while idle: 50 ms worth of data.
const BURST_NS: u64 = 50_000_000;
/// Longest single sleep while throttled, so changes and cancellation are noticed quickly.
const SLICE: Duration = Duration::from_millis(20);
/// How often a parked worker looks at the limit again.
#[cfg_attr(not(feature = "zip"), allow(dead_code))]
const PARK: Duration = Duration::from_millis(20);

/// The handle to a running operation: cancellation plus values that can change while it runs.
///
/// Clones share the same state and may be used from any thread.
#[derive(Debug, Clone)]
pub struct Control(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    cancelled: AtomicBool,
    /// Thread limit; 0 means "what the profile says".
    threads: AtomicUsize,
    /// 0 means "what the profile says", otherwise 1 + the priority.
    priority: AtomicU8,
    /// Last profile given to `set_profile`; 0 if none.
    profile: AtomicU8,
    /// Throughput limit in bytes per second; 0 means none.
    rate: AtomicU64,
    /// Token bucket in virtual time: when it runs empty, in nanoseconds since `epoch`.
    empty_at: AtomicU64,
    epoch: Instant,
}

impl Default for Control {
    fn default() -> Self {
        Self::new()
    }
}

impl Control {
    /// A control with nothing cancelled, limited or switched.
    pub fn new() -> Self {
        Control(Arc::new(Inner {
            cancelled: AtomicBool::new(false),
            threads: AtomicUsize::new(0),
            priority: AtomicU8::new(0),
            profile: AtomicU8::new(0),
            rate: AtomicU64::new(0),
            empty_at: AtomicU64::new(0),
            epoch: Instant::now(),
        }))
    }

    /// Request cancellation. Safe to call from any thread or a signal handler thread.
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Relaxed);
    }

    /// Whether [`cancel`](Self::cancel) has been called.
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Relaxed)
    }

    /// Limits the workers that may run at once, from 1 up to what the operation started with.
    /// Workers above the limit wait until it rises again. `0` goes back to the profile's value.
    pub fn set_threads(&self, threads: usize) {
        self.0.threads.store(threads, Relaxed);
    }

    /// The limit set with [`set_threads`](Self::set_threads); `0` if the profile decides.
    pub fn threads(&self) -> usize {
        self.0.threads.load(Relaxed)
    }

    /// Sets the priority that [`WorkerHook`] is told about.
    pub fn set_priority(&self, priority: Priority) {
        self.0.priority.store(priority as u8 + 1, Relaxed);
    }

    /// The priority set with [`set_priority`](Self::set_priority); `None` if the profile decides.
    pub fn priority(&self) -> Option<Priority> {
        match self.0.priority.load(Relaxed) {
            1 => Some(Priority::Normal),
            2 => Some(Priority::Low),
            _ => None,
        }
    }

    /// Limits the bytes processed per second, over all workers together. `0` removes the limit.
    pub fn set_rate_limit(&self, bytes_per_second: u64) {
        self.0.rate.store(bytes_per_second, Relaxed);
        self.0.empty_at.store(0, Relaxed);
    }

    /// The limit set with [`set_rate_limit`](Self::set_rate_limit); `0` means none.
    pub fn rate_limit(&self) -> u64 {
        self.0.rate.load(Relaxed)
    }

    /// Switches to the thread limit and priority of `profile`.
    ///
    /// `Auto` is resolved before an operation starts (see [`Profile::resolve`]); passing it
    /// here changes nothing. The drive-dependent settings (buffer size, parallel extraction)
    /// are fixed when the operation starts.
    pub fn set_profile(&self, profile: Profile) {
        let code = match profile {
            Profile::Fast => 1,
            Profile::Balanced => 2,
            Profile::Gentle => 3,
            Profile::Auto => return,
        };
        let (threads, priority) = cpu_rules(profile, cpus());
        self.set_threads(threads);
        self.set_priority(priority);
        self.0.profile.store(code, Relaxed);
    }

    /// The last profile given to [`set_profile`](Self::set_profile), if any.
    pub fn profile(&self) -> Option<Profile> {
        match self.0.profile.load(Relaxed) {
            1 => Some(Profile::Fast),
            2 => Some(Profile::Balanced),
            3 => Some(Profile::Gentle),
            _ => None,
        }
    }

    /// Accounts for `bytes` just processed and sleeps if the throughput limit says so.
    ///
    /// With no limit this is one atomic load. Called between chunks, never inside one.
    pub(crate) fn throttle(&self, bytes: u64) {
        let rate = self.0.rate.load(Relaxed);
        if rate == 0 {
            return;
        }
        let cost = u128::from(bytes) * 1_000_000_000 / u128::from(rate);
        let cost = u64::try_from(cost).unwrap_or(u64::MAX);
        let now = u64::try_from(self.0.epoch.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let mut seen = self.0.empty_at.load(Relaxed);
        let wait = loop {
            let (empty_at, wait) = reserve(seen, now, cost);
            match self
                .0
                .empty_at
                .compare_exchange_weak(seen, empty_at, Relaxed, Relaxed)
            {
                Ok(_) => break wait,
                Err(current) => seen = current,
            }
        };
        let mut left = Duration::from_nanos(wait);
        // A changed limit or a cancel ends the wait: the new rate applies from the next chunk.
        while !left.is_zero() && !self.is_cancelled() && self.rate_limit() == rate {
            let step = left.min(SLICE);
            std::thread::sleep(step);
            left -= step;
        }
    }

    /// Tells `hook` about this thread's priority if it has not heard it yet. `base` is used
    /// while no priority was set on the control.
    pub(crate) fn sync_priority(&self, hook: &WorkerHook, base: Priority) {
        thread_local! {
            /// What the hook last heard on this thread: (which control, priority + 1).
            static HEARD: Cell<(usize, u8)> = const { Cell::new((0, 0)) };
        }
        let priority = self.priority().unwrap_or(base);
        let heard = (Arc::as_ptr(&self.0) as usize, priority as u8 + 1);
        HEARD.with(|last| {
            if last.get() != heard {
                last.set(heard);
                hook(priority);
            }
        });
    }
}

/// Books `cost` nanoseconds of throughput on a bucket that runs empty at `empty_at`, at time
/// `now`. Returns the new `empty_at` and how long to wait. Idle time earns at most
/// [`BURST_NS`] of credit, so a pause does not allow a long burst afterwards.
fn reserve(empty_at: u64, now: u64, cost: u64) -> (u64, u64) {
    let end = empty_at
        .max(now.saturating_sub(BURST_NS))
        .saturating_add(cost);
    (end, end.saturating_sub(now))
}

/// An operation's workers as one value: how many exist, how many may run, the priority.
///
/// Resolved from [`Options`](crate::Options) when the operation starts; the live part is read
/// from the shared [`Control`].
pub(crate) struct Workers {
    control: Control,
    hook: Option<WorkerHook>,
    /// Workers to create: the most that can ever run.
    pub(crate) pool: usize,
    /// How many may run while the control has no limit of its own.
    limit: usize,
    /// Priority to use while the control has none of its own.
    priority: Priority,
}

impl Workers {
    pub(crate) fn new(
        control: Control,
        hook: Option<WorkerHook>,
        pool: usize,
        limit: usize,
        priority: Priority,
    ) -> Self {
        Workers {
            control,
            hook,
            pool: pool.max(1),
            limit,
            priority,
        }
    }

    /// How many workers may run right now: at least 1, at most the pool.
    pub(crate) fn active(&self) -> usize {
        match self.control.threads() {
            0 => self.limit,
            limit => limit,
        }
        .clamp(1, self.pool)
    }

    /// Lets the hook know the priority of the calling thread. A no-op without a hook.
    pub(crate) fn sync_priority(&self) {
        if let Some(hook) = &self.hook {
            self.control.sync_priority(hook, self.priority);
        }
    }

    /// The safe point of a long-lived worker, called between files.
    ///
    /// Fails through `stop` when the operation is cancelled or aborted. Worker `index` waits
    /// here while it is above the limit, until the limit rises, `stop` fails or `finished`
    /// says there is nothing left to do (so a worker that was waiting cannot outlive the work).
    /// Worker 0 never waits.
    #[cfg_attr(not(feature = "zip"), allow(dead_code))]
    pub(crate) fn checkpoint(
        &self,
        index: usize,
        stop: impl Fn() -> Result<()>,
        finished: impl Fn() -> bool,
    ) -> Result<()> {
        stop()?;
        self.sync_priority();
        while index >= self.active() && !finished() {
            std::thread::sleep(PARK);
            stop()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use std::sync::Mutex;

    #[test]
    fn the_bucket_allows_a_burst_then_paces() {
        let ms = 1_000_000;
        // Idle for a long time: a small request goes through at once.
        assert_eq!(reserve(0, 1000 * ms, 10 * ms), (960 * ms, 0));
        // The next one still fits in the credit.
        assert_eq!(reserve(960 * ms, 1000 * ms, 10 * ms), (970 * ms, 0));
        // Once the bucket is ahead of the clock, requests wait for their slot.
        assert_eq!(
            reserve(1200 * ms, 1000 * ms, 10 * ms),
            (1210 * ms, 210 * ms)
        );
        // A request bigger than the credit waits for the difference.
        assert_eq!(reserve(0, 1000 * ms, 100 * ms), (1050 * ms, 50 * ms));
        // Absurd costs saturate instead of wrapping.
        assert_eq!(reserve(u64::MAX, 5, u64::MAX).0, u64::MAX);
    }

    #[test]
    fn no_limit_never_sleeps_and_a_limit_can_be_lifted() {
        let c = Control::new();
        c.throttle(u64::MAX); // would be hours of sleep if it counted
        c.set_rate_limit(1_000_000);
        assert_eq!(c.rate_limit(), 1_000_000);
        c.set_rate_limit(0);
        assert_eq!(c.rate_limit(), 0);
    }

    #[test]
    fn profile_switch_sets_threads_and_priority() {
        let c = Control::new();
        assert_eq!((c.threads(), c.priority(), c.profile()), (0, None, None));
        c.set_profile(Profile::Gentle);
        assert_eq!(c.priority(), Some(Priority::Low));
        assert!((1..=2).contains(&c.threads()));
        assert_eq!(c.profile(), Some(Profile::Gentle));
        c.set_profile(Profile::Fast);
        assert_eq!(c.priority(), Some(Priority::Normal));
        assert_eq!(c.threads(), cpus());
        // Auto must be resolved by the caller: here it changes nothing.
        c.set_profile(Profile::Auto);
        assert_eq!(c.profile(), Some(Profile::Fast));
        c.set_threads(0);
        assert_eq!(c.threads(), 0);
    }

    #[test]
    fn active_follows_the_control_within_the_pool() {
        let c = Control::new();
        let w = Workers::new(c.clone(), None, 4, 2, Priority::Normal);
        assert_eq!(w.active(), 2);
        c.set_threads(9);
        assert_eq!(w.active(), 4);
        c.set_threads(1);
        assert_eq!(w.active(), 1);
        c.set_threads(0);
        assert_eq!(w.active(), 2);
        assert_eq!(Workers::new(c, None, 0, 0, Priority::Normal).active(), 1);
    }

    #[test]
    fn the_hook_hears_each_change_once_per_thread() {
        let heard = Arc::new(Mutex::new(Vec::new()));
        let log = heard.clone();
        let hook: WorkerHook = Arc::new(move |p| log.lock().unwrap().push(p));
        let c = Control::new();
        let w = Workers::new(c.clone(), Some(hook), 2, 2, Priority::Normal);

        w.sync_priority();
        w.sync_priority();
        c.set_priority(Priority::Low);
        w.sync_priority();
        w.sync_priority();
        assert_eq!(*heard.lock().unwrap(), [Priority::Normal, Priority::Low]);

        // A new thread has heard nothing yet.
        std::thread::scope(|s| {
            s.spawn(|| w.sync_priority());
        });
        assert_eq!(heard.lock().unwrap().len(), 3);
    }

    #[test]
    fn a_waiting_worker_gives_way_to_cancel_and_to_the_end_of_the_work() {
        let c = Control::new();
        let w = Workers::new(c.clone(), None, 4, 1, Priority::Normal);
        let ok = || Ok(());
        // Worker 0 is never held; worker 3 is above the limit but there is nothing left.
        assert!(w.checkpoint(0, ok, || false).is_ok());
        assert!(w.checkpoint(3, ok, || true).is_ok());
        // Cancelled: the check at the top fails at once.
        c.cancel();
        let stop = || {
            if c.is_cancelled() {
                Err(Error::Cancelled)
            } else {
                Ok(())
            }
        };
        assert!(matches!(
            w.checkpoint(3, stop, || false),
            Err(Error::Cancelled)
        ));
    }
}
