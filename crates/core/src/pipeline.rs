//! An ordered work pipeline with a byte budget.
//!
//! The calling thread reads items from a source, in order, and hands them to worker
//! threads. Finished results are given back to a sink on the calling thread, again in the
//! order the source produced them. Unlike running a batch and waiting for its slowest item,
//! workers keep taking the next item as soon as they are free; a slow item only delays
//! the sink, not the other workers, until the budget is spent.
//!
//! What is in flight is bounded by bytes: every job states the most memory it may hold
//! and reserves it before the source is read any further. When a job ends, its reservation
//! shrinks to what it actually keeps, and is released once the sink has consumed the
//! result. Items the sink must handle itself (a file too big to hold, a directory) take a
//! place in the order too, but no budget.
//!
//! Nothing here knows about archive formats.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::Scope;
use std::time::Duration;

use crate::control::Workers;
use crate::error::{Error, Result};
use crate::stats::{self, Class};

/// How often a waiting thread looks at cancellation.
const POLL: Duration = Duration::from_millis(10);

/// What the source hands the pipeline.
pub(crate) enum Item<J, I> {
    /// Run on a worker. `cost` is the most memory the job holds at any time, in bytes.
    Work { job: J, cost: u64 },
    /// Handled by the sink itself, on the calling thread, at its place in the order.
    ///
    /// `defer` says the sink is slow with it (a big file): the source is read ahead first,
    /// until the budget is full, so the workers have something to do meanwhile.
    Inline { item: I, defer: bool },
}

/// What the sink receives.
pub(crate) enum Out<R, I> {
    Done(R),
    Inline(I),
}

pub(crate) struct Config<'a> {
    pub workers: &'a Workers,
    /// Bytes that may be reserved at once. A job bigger than this still runs, alone.
    pub budget: u64,
    /// Most items, of any kind, between the source and the sink.
    pub max_items: usize,
}

enum Body<R, I> {
    Pending,
    Done(Result<R>),
    Inline(I, bool),
}

struct Slot<R, I> {
    /// Bytes reserved for this item.
    cost: u64,
    body: Body<R, I>,
}

struct State<J, R, I> {
    /// Sequence number of `slots[0]`.
    base: u64,
    slots: VecDeque<Slot<R, I>>,
    queue: VecDeque<(u64, J)>,
    reserved: u64,
    /// Jobs queued or running.
    outstanding: usize,
    spawned: usize,
    closed: bool,
    /// A job failed: nothing new is started, and the sink reaches the error in order.
    failed: bool,
    /// A worker was stopped (cancelled): reported at once.
    fatal: Option<Error>,
}

struct Shared<J, R, I> {
    state: Mutex<State<J, R, I>>,
    /// A job was queued, or the pipeline closed.
    work: Condvar,
    /// The first slot may have become ready.
    ready: Condvar,
}

impl<J, R, I> Shared<J, R, I> {
    fn lock(&self) -> MutexGuard<'_, State<J, R, I>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn close(&self) {
        self.lock().closed = true;
        self.work.notify_all();
    }

    fn fatal(&self, e: Error) {
        self.lock().fatal.get_or_insert(e);
        self.ready.notify_all();
    }

    fn is_over(&self) -> bool {
        let st = self.lock();
        st.closed || st.failed
    }

    /// The next job, waiting for one; `None` when the pipeline is over.
    fn claim(&self) -> Option<(u64, J)> {
        let mut st = self.lock();
        loop {
            if st.closed || st.failed {
                return None;
            }
            if let Some(job) = st.queue.pop_front() {
                return Some(job);
            }
            st = self.work.wait(st).unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn finish(&self, seq: u64, result: Result<(R, u64)>) {
        let mut st = self.lock();
        let index = (seq - st.base) as usize;
        let slot = &mut st.slots[index];
        let old = slot.cost;
        let (body, kept) = match result {
            Ok((value, kept)) => (Body::Done(Ok(value)), kept),
            Err(e) => (Body::Done(Err(e)), 0),
        };
        let failed = matches!(body, Body::Done(Err(_)));
        slot.cost = kept;
        slot.body = body;
        st.reserved = st.reserved.saturating_sub(old).saturating_add(kept);
        st.outstanding -= 1;
        if failed {
            st.failed = true;
            self.work.notify_all();
        }
        if index == 0 {
            self.ready.notify_all();
        }
    }

    /// Queues `item`. Gives it back when the budget or the item limit has no room for it.
    /// On success returns the index of a worker to start, if one is wanted.
    fn push(
        &self,
        item: Item<J, I>,
        cfg: &Config<'_>,
    ) -> std::result::Result<Option<usize>, Item<J, I>> {
        let mut st = self.lock();
        if st.slots.len() >= cfg.max_items {
            return Err(item);
        }
        let seq = st.base + st.slots.len() as u64;
        let mut spawn = None;
        match item {
            Item::Work { job, cost } => {
                // An empty window always admits: otherwise a big job could never run.
                if !st.slots.is_empty() && st.reserved.saturating_add(cost) > cfg.budget {
                    return Err(Item::Work { job, cost });
                }
                st.reserved = st.reserved.saturating_add(cost);
                st.slots.push_back(Slot {
                    cost,
                    body: Body::Pending,
                });
                st.queue.push_back((seq, job));
                st.outstanding += 1;
                if st.spawned < st.outstanding.min(cfg.workers.active()) {
                    spawn = Some(st.spawned);
                    st.spawned += 1;
                }
                self.work.notify_one();
            }
            Item::Inline { item, defer } => st.slots.push_back(Slot {
                cost: 0,
                body: Body::Inline(item, defer),
            }),
        }
        if let Some(s) = &cfg.workers.stats {
            s.note_queue(st.slots.len());
        }
        Ok(spawn)
    }

    fn is_empty(&self) -> bool {
        self.lock().slots.is_empty()
    }

    fn has_item_room(&self, cfg: &Config<'_>) -> bool {
        let st = self.lock();
        !st.failed && st.slots.len() < cfg.max_items
    }

    /// Takes the first slot if it is ready: a result, or an inline item that need not wait
    /// (`force` takes any inline item).
    fn pop(st: &mut State<J, R, I>, force: bool) -> Option<Result<(Out<R, I>, u64)>> {
        let ready = match &st.slots.front()?.body {
            Body::Pending => false,
            Body::Done(_) => true,
            Body::Inline(_, defer) => force || !defer,
        };
        if !ready {
            return None;
        }
        let slot = st.slots.pop_front()?;
        st.base += 1;
        Some(match slot.body {
            Body::Done(Ok(r)) => Ok((Out::Done(r), slot.cost)),
            Body::Done(Err(e)) => Err(e),
            Body::Inline(i, _) => Ok((Out::Inline(i), slot.cost)),
            Body::Pending => unreachable!("a pending slot is never ready"),
        })
    }

    fn try_pop(&self) -> Option<Result<(Out<R, I>, u64)>> {
        Self::pop(&mut self.lock(), false)
    }

    /// The first slot, waiting for it to be ready; `None` when there are no slots.
    fn wait_pop(
        &self,
        stop: &(dyn Fn() -> Result<()> + Sync),
        stats: Option<&stats::Stats>,
    ) -> Result<Option<(Out<R, I>, u64)>> {
        loop {
            {
                let mut st = self.lock();
                if let Some(e) = st.fatal.take() {
                    return Err(e);
                }
                if st.slots.is_empty() {
                    return Ok(None);
                }
                if let Some(slot) = Self::pop(&mut st, true) {
                    return slot.map(Some);
                }
                // The calling thread has nothing to do until the first job is done.
                stats::timed(stats, Class::Workers, || {
                    drop(
                        self.ready
                            .wait_timeout(st, POLL)
                            .unwrap_or_else(PoisonError::into_inner),
                    );
                });
            }
            stop()?;
        }
    }

    fn release(&self, bytes: u64) {
        let mut st = self.lock();
        st.reserved = st.reserved.saturating_sub(bytes);
    }
}

/// Closes the pipeline when the calling thread leaves for any reason, so workers end.
struct Closer<'a, J, R, I>(&'a Shared<J, R, I>);

impl<J, R, I> Drop for Closer<'_, J, R, I> {
    fn drop(&mut self) {
        self.0.close();
    }
}

fn worker<J, R, I>(
    shared: &Shared<J, R, I>,
    work: &(dyn Fn(J) -> Result<(R, u64)> + Sync),
    stop: &(dyn Fn() -> Result<()> + Sync),
    workers: &Workers,
    index: usize,
) {
    let _lane = stats::lane(workers.stats.as_ref());
    loop {
        if let Err(e) = workers.checkpoint(index, stop, || shared.is_over()) {
            shared.fatal(e);
            return;
        }
        // A worker without a job is waiting for the source.
        let claimed = stats::timed(workers.stats.as_ref(), Class::Input, || shared.claim());
        let Some((seq, job)) = claimed else { return };
        shared.finish(seq, work(job));
    }
}

/// Runs the pipeline until the source ends or something fails.
///
/// * `source` is called on the calling thread, only while there is room.
/// * `work` runs on workers; it returns its result and the bytes the result keeps.
/// * `sink` receives results and inline items in source order, on the calling thread.
/// * `stop` is polled by every thread; an error from it ends the run.
///
/// The first failure in source order is the one returned; a failed job stops new ones
/// from starting.
pub(crate) fn run<J, R, I>(
    cfg: &Config<'_>,
    stop: &(dyn Fn() -> Result<()> + Sync),
    mut source: impl FnMut() -> Result<Option<Item<J, I>>>,
    work: &(dyn Fn(J) -> Result<(R, u64)> + Sync),
    mut sink: impl FnMut(Out<R, I>) -> Result<()>,
) -> Result<()>
where
    J: Send,
    R: Send,
    I: Send,
{
    let shared = Shared {
        state: Mutex::new(State {
            base: 0,
            slots: VecDeque::new(),
            queue: VecDeque::new(),
            reserved: 0,
            outstanding: 0,
            spawned: 0,
            closed: false,
            failed: false,
            fatal: None,
        }),
        work: Condvar::new(),
        ready: Condvar::new(),
    };
    let stats = cfg.workers.stats.as_ref();
    std::thread::scope(|scope: &Scope<'_, '_>| {
        let _closer = Closer(&shared);
        let shared = &shared;
        let mut held: Option<Item<J, I>> = None;
        let mut source_done = false;
        loop {
            stop()?;
            // Whatever is already finished goes out first: it frees budget.
            while let Some(out) = shared.try_pop() {
                let (out, cost) = out?;
                sink(out)?;
                shared.release(cost);
            }
            if held.is_none() && !source_done && shared.has_item_room(cfg) {
                match source()? {
                    Some(item) => held = Some(item),
                    None => source_done = true,
                }
            }
            if let Some(item) = held.take() {
                match shared.push(item, cfg) {
                    Ok(start) => {
                        if let Some(index) = start {
                            scope.spawn(move || worker(shared, work, stop, cfg.workers, index));
                        }
                        continue;
                    }
                    Err(item) => held = Some(item),
                }
            }
            if source_done && held.is_none() && shared.is_empty() {
                return Ok(());
            }
            // The budget is spent or the source is over: wait for the first item.
            match shared.wait_pop(stop, stats)? {
                Some((out, cost)) => {
                    sink(out)?;
                    shared.release(cost);
                }
                None if source_done && held.is_none() => return Ok(()),
                None => {}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{Control, Priority};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::time::Instant;

    fn workers(pool: usize) -> Workers {
        Workers::new(Control::default(), None, pool, pool, Priority::Normal, None)
    }

    fn never() -> Result<()> {
        Ok(())
    }

    #[test]
    fn a_slow_writer_and_a_slow_worker_keep_the_budget_and_the_order() {
        const BUDGET: u64 = 1000;
        const ITEMS: u64 = 200;
        let w = workers(4);
        let cfg = Config {
            workers: &w,
            budget: BUDGET,
            max_items: 1 << 20,
        };
        // Bytes between "the source handed it out" and "the sink consumed it".
        let held = AtomicU64::new(0);
        let peak = AtomicU64::new(0);
        let next = AtomicU64::new(0);
        let order = Mutex::new(Vec::new());
        let source = || {
            let n = next.fetch_add(1, Ordering::SeqCst);
            if n == ITEMS {
                return Ok(None);
            }
            // Costs between 10 and 330 bytes; every 7th item is the slow one.
            let cost = 10 + (n * 37) % 321;
            let now = held.fetch_add(cost, Ordering::SeqCst) + cost;
            peak.fetch_max(now, Ordering::SeqCst);
            Ok(Some(Item::Work {
                job: (n, cost),
                cost,
            }))
        };
        let work = |(n, cost): (u64, u64)| {
            if n % 7 == 0 {
                std::thread::sleep(Duration::from_millis(15));
            }
            Ok(((n, cost), cost))
        };
        let sink = |out: Out<(u64, u64), ()>| {
            let Out::Done((n, cost)) = out else {
                panic!("no inline items here")
            };
            // The slow writer.
            std::thread::sleep(Duration::from_millis(1));
            order.lock().unwrap().push(n);
            held.fetch_sub(cost, Ordering::SeqCst);
            Ok(())
        };
        run(&cfg, &never, source, &work, sink).unwrap();
        assert_eq!(*order.lock().unwrap(), (0..ITEMS).collect::<Vec<_>>());
        // The item being admitted is counted before the pipeline can refuse it, so the
        // bound is the budget plus that one held item (at most 330 bytes).
        assert!(
            peak.load(Ordering::SeqCst) <= BUDGET + 330,
            "peak {} over budget {BUDGET}",
            peak.load(Ordering::SeqCst)
        );
        // The budget really was the limit: far more than one item was in flight.
        assert!(peak.load(Ordering::SeqCst) > BUDGET / 2);
    }

    #[test]
    fn a_slow_item_does_not_stop_the_others() {
        let w = workers(4);
        let cfg = Config {
            workers: &w,
            budget: u64::MAX,
            max_items: 1 << 20,
        };
        let next = AtomicUsize::new(0);
        let source = || {
            let n = next.fetch_add(1, Ordering::SeqCst);
            Ok((n < 40).then_some(Item::Work { job: n, cost: 1 }))
        };
        // Item 0 takes as long as the other 39 together would on one worker.
        let work = |n: usize| {
            std::thread::sleep(Duration::from_millis(if n == 0 { 120 } else { 10 }));
            Ok((n, 1))
        };
        let mut seen = Vec::new();
        let start = Instant::now();
        run(&cfg, &never, source, &work, |out: Out<usize, ()>| {
            if let Out::Done(n) = out {
                seen.push(n);
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, (0..40).collect::<Vec<_>>());
        // 39 items * 10 ms on 3 free workers is ~130 ms; waves behind item 0 would need 120+.
        assert!(
            start.elapsed() < Duration::from_millis(330),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn inline_items_keep_their_place_and_a_deferred_one_lets_the_source_run_ahead() {
        let w = workers(2);
        let cfg = Config {
            workers: &w,
            budget: 1 << 20,
            max_items: 64,
        };
        let next = AtomicUsize::new(0);
        let sourced = AtomicUsize::new(0);
        let source = || {
            let n = next.fetch_add(1, Ordering::SeqCst);
            sourced.store(n, Ordering::SeqCst);
            Ok(match n {
                0 | 5 => Some(Item::Work { job: n, cost: 1 }),
                1 => Some(Item::Inline {
                    item: n,
                    defer: true,
                }),
                2 => Some(Item::Inline {
                    item: n,
                    defer: false,
                }),
                3..=4 | 6..=7 => Some(Item::Work { job: n, cost: 1 }),
                _ => None,
            })
        };
        let mut ahead_at_deferred = 0;
        let mut order = Vec::new();
        run(&cfg, &never, source, &(|n| Ok((n, 1))), |out| {
            match out {
                Out::Done(n) => order.push(n),
                Out::Inline(n) => {
                    if n == 1 {
                        ahead_at_deferred = sourced.load(Ordering::SeqCst);
                    }
                    order.push(n);
                }
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(order, [0, 1, 2, 3, 4, 5, 6, 7]);
        // The deferred item was sunk only once the source had been read to its end.
        assert!(ahead_at_deferred >= 8, "{ahead_at_deferred}");
    }

    #[test]
    fn the_first_failure_in_order_is_returned_and_later_work_is_not_started() {
        let w = workers(3);
        let cfg = Config {
            workers: &w,
            budget: u64::MAX,
            max_items: 1 << 20,
        };
        let next = AtomicUsize::new(0);
        let started = AtomicUsize::new(0);
        let source = || {
            let n = next.fetch_add(1, Ordering::SeqCst);
            Ok((n < 1000).then_some(Item::Work { job: n, cost: 1 }))
        };
        let work = |n: usize| {
            started.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(1));
            if n == 10 {
                Err(Error::InvalidOption("ten".into()))
            } else {
                Ok((n, 1))
            }
        };
        let mut sunk = Vec::new();
        let err = run(&cfg, &never, source, &work, |out: Out<usize, ()>| {
            if let Out::Done(n) = out {
                sunk.push(n);
            }
            Ok(())
        })
        .unwrap_err();
        assert!(matches!(err, Error::InvalidOption(m) if m == "ten"));
        assert_eq!(sunk, (0..10).collect::<Vec<_>>());
        assert!(started.load(Ordering::SeqCst) < 100);
    }

    #[test]
    fn cancelling_stops_within_milliseconds_even_with_a_stuck_head() {
        let w = workers(2);
        let cfg = Config {
            workers: &w,
            budget: 10,
            max_items: 1 << 20,
        };
        let begun = Instant::now();
        let stop = || {
            if begun.elapsed() > Duration::from_millis(50) {
                Err(Error::Cancelled)
            } else {
                Ok(())
            }
        };
        let next = AtomicUsize::new(0);
        let source = || {
            let n = next.fetch_add(1, Ordering::SeqCst);
            Ok(Some(Item::Work { job: n, cost: 10 }))
        };
        // Every job outlives the test: the sink would wait for the first one for 400 ms.
        let work = |n: usize| {
            std::thread::sleep(Duration::from_millis(400));
            Ok((n, 10))
        };
        let err = run(&cfg, &stop, source, &work, |_: Out<usize, ()>| Ok(())).unwrap_err();
        assert!(matches!(err, Error::Cancelled));
        // The workers are joined by the scope, so the whole call includes their 400 ms
        // job; the point is that the calling thread noticed at once (it did not start a
        // second round of work).
        assert!(next.load(Ordering::SeqCst) <= 3);
    }
}
