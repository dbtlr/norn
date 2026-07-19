//! Per-vault writer queue — the single serialization point for all write-shaped
//! work against one vault (ADR 0013 Phase 2).
//!
//! One dedicated OS thread per vault owns all write work — generation opens, the
//! freshness refresh, and (phase 3) mutation-increment commits — and a two-class
//! schedule keeps latency-critical work ahead of throughput work.
//!
//! # Two submission classes
//!
//! - **Liveness** ([`WriterQueue::submit_liveness`]) — work a reader is blocked
//!   on. Runs FIFO among liveness ops and preempts bulk at the next chunk
//!   boundary. A liveness op is a plain `FnOnce() -> R`.
//! - **Bulk** ([`WriterQueue::submit_bulk`]) — throughput work no one is
//!   synchronously waiting on. A bulk op is a *chunked* closure called repeatedly,
//!   each call returning [`ChunkOutcome::More`] or [`ChunkOutcome::Done`]; between
//!   chunks the queue drains the entire liveness queue.
//!
//! # Drop-on-death, shutdown, panic safety
//!
//! [`submit_bulk`](WriterQueue::submit_bulk) takes an optional `still_valid`
//! predicate checked at every chunk boundary; when it turns false the op is
//! dropped and its handle resolves to [`Outcome::Dropped`]. Dropping the queue
//! finishes the current chunk, drops queued ops, and joins the thread. Each op
//! runs inside [`catch_unwind`]; a panic resolves that op's handle to
//! [`Outcome::Panicked`] and the writer keeps serving.

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

/// A validity predicate for a bulk op, re-checked at every chunk boundary.
pub(crate) type ValidityGuard = Box<dyn Fn() -> bool + Send>;

/// A type-erased liveness op. The boolean says whether this is top-level work
/// that owns the queue's busy transition (`true`) or preemption inside an
/// already-busy bulk op (`false`).
type LivenessJob = Box<dyn FnOnce(bool) + Send>;

/// One step of a chunked bulk op.
pub enum ChunkOutcome<R> {
    /// Further chunks remain; the queue re-invokes the op after a preemption check.
    More,
    /// The op finished; carries the result delivered to the submitter's handle.
    Done(R),
}

/// How a submitted op ultimately resolved, observed via [`Handle::wait`].
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome<R> {
    /// The op ran to completion; carries its return value.
    Done(R),
    /// The op was abandoned without completing (guard turned false, or shutdown).
    Dropped,
    /// The op panicked; the queue caught the unwind and kept serving.
    Panicked,
}

/// Coherent control-plane snapshot of one per-vault writer queue.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WriterProgress {
    pub busy: bool,
    pub sequence: u64,
}

#[derive(Debug, Default)]
struct ProgressValue {
    active: u64,
    sequence: u64,
}

/// Per-vault progress that outlives any disposable warm context.
#[derive(Debug, Default)]
pub struct WriterProgressState {
    value: Mutex<ProgressValue>,
}

impl WriterProgressState {
    fn begin_work(&self) {
        let mut value = self.value.lock().unwrap_or_else(|e| e.into_inner());
        value.sequence = value.sequence.saturating_add(1);
        value.active = value.active.saturating_add(1);
    }

    fn advance_progress(&self) {
        let mut value = self.value.lock().unwrap_or_else(|e| e.into_inner());
        value.sequence = value.sequence.saturating_add(1);
    }

    fn finish_work(&self) {
        let mut value = self.value.lock().unwrap_or_else(|e| e.into_inner());
        value.sequence = value.sequence.saturating_add(1);
        value.active = value.active.saturating_sub(1);
    }

    pub fn snapshot(&self) -> WriterProgress {
        let value = self.value.lock().unwrap_or_else(|e| e.into_inner());
        WriterProgress {
            busy: value.active > 0,
            sequence: value.sequence,
        }
    }
}

/// A blocking handle to a submitted op's eventual [`Outcome`].
#[must_use = "a submitted op's outcome should be awaited via `wait`"]
pub struct Handle<R> {
    rx: mpsc::Receiver<Outcome<R>>,
}

impl<R> Handle<R> {
    /// Block until the op resolves. A disconnected channel (dropped on shutdown)
    /// resolves to [`Outcome::Dropped`] rather than hanging.
    pub fn wait(self) -> Outcome<R> {
        self.rx.recv().unwrap_or(Outcome::Dropped)
    }
}

struct State {
    liveness: VecDeque<LivenessJob>,
    bulk: VecDeque<Box<dyn BulkJob>>,
}

struct Inner {
    state: Mutex<State>,
    signal: Condvar,
    shutdown: AtomicBool,
    progress: Arc<WriterProgressState>,
}

impl Inner {
    fn advance_progress(&self) {
        self.progress.advance_progress();
    }

    fn begin_work(&self) {
        self.progress.begin_work();
    }

    #[cfg(test)]
    fn progress(&self) -> WriterProgress {
        self.progress.snapshot()
    }

    fn drain_liveness(&self) {
        loop {
            let job = {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if self.shutdown.load(Ordering::Acquire) {
                    return;
                }
                state.liveness.pop_front()
            };
            match job {
                Some(job) => {
                    self.advance_progress();
                    job(false);
                }
                None => return,
            }
        }
    }
}

trait BulkJob: Send {
    fn valid(&self) -> bool;
    fn run_chunk(&mut self) -> bool;
    fn deliver_dropped(&mut self);
}

struct BulkClosure<R, F> {
    step: F,
    still_valid: Option<ValidityGuard>,
    progress: Arc<WriterProgressState>,
    tx: Option<mpsc::Sender<Outcome<R>>>,
}

impl<R, F> BulkClosure<R, F> {
    fn resolve(&mut self, outcome: Outcome<R>) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(outcome);
        }
    }
}

impl<R, F> BulkJob for BulkClosure<R, F>
where
    R: Send,
    F: FnMut() -> ChunkOutcome<R> + Send,
{
    fn valid(&self) -> bool {
        match &self.still_valid {
            None => true,
            Some(guard) => catch_unwind(AssertUnwindSafe(guard)).unwrap_or(false),
        }
    }

    fn run_chunk(&mut self) -> bool {
        let step = &mut self.step;
        match catch_unwind(AssertUnwindSafe(step)) {
            Ok(ChunkOutcome::More) => {
                self.progress.advance_progress();
                true
            }
            Ok(ChunkOutcome::Done(result)) => {
                self.progress.advance_progress();
                self.progress.finish_work();
                self.resolve(Outcome::Done(result));
                false
            }
            Err(_) => {
                self.progress.advance_progress();
                self.progress.finish_work();
                self.resolve(Outcome::Panicked);
                false
            }
        }
    }

    fn deliver_dropped(&mut self) {
        self.progress.finish_work();
        self.resolve(Outcome::Dropped);
    }
}

enum Pick {
    Liveness(LivenessJob),
    Bulk(Box<dyn BulkJob>),
}

/// A per-vault writer queue owning one dedicated OS thread. Write work is
/// blocking rusqlite / filesystem work, so this is a `std::thread`.
pub struct WriterQueue {
    inner: Arc<Inner>,
    worker: Option<JoinHandle<()>>,
}

impl WriterQueue {
    /// Spawn the queue with fresh per-vault progress state. `name` labels the OS
    /// thread only.
    pub fn spawn(name: &str) -> WriterQueue {
        Self::spawn_with_progress(name, Arc::new(WriterProgressState::default()))
    }

    /// Spawn a queue backed by owner-lifetime per-vault progress state.
    pub fn spawn_with_progress(name: &str, progress: Arc<WriterProgressState>) -> WriterQueue {
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                liveness: VecDeque::new(),
                bulk: VecDeque::new(),
            }),
            signal: Condvar::new(),
            shutdown: AtomicBool::new(false),
            progress,
        });
        let worker_inner = Arc::clone(&inner);
        let worker = std::thread::Builder::new()
            .name(format!("norn-writer-queue:{name}"))
            .spawn(move || worker_loop(&worker_inner))
            .expect("failed to spawn writer-queue thread");
        WriterQueue {
            inner,
            worker: Some(worker),
        }
    }

    #[cfg(test)]
    pub(crate) fn progress(&self) -> WriterProgress {
        self.inner.progress()
    }

    /// A coherent snapshot of this queue's writer progress (ADR 0013 control
    /// plane). Owner-facing: the summoned owner reports `{ busy, sequence }` in
    /// a scoped pong so a client can tell a healthy busy writer (sequence
    /// advancing) from a hung one (sequence stalled past the stall budget).
    pub fn progress_snapshot(&self) -> WriterProgress {
        self.inner.progress.snapshot()
    }

    /// Submit a liveness op — latency-critical work a reader is blocked on.
    pub fn submit_liveness<R, F>(&self, op: F) -> Handle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let progress = Arc::clone(&self.inner.progress);
        let job: LivenessJob = Box::new(move |owns_busy| {
            let outcome = match catch_unwind(AssertUnwindSafe(op)) {
                Ok(result) => Outcome::Done(result),
                Err(_) => Outcome::Panicked,
            };
            if owns_busy {
                progress.finish_work();
            } else {
                progress.advance_progress();
            }
            let _ = tx.send(outcome);
        });
        let enqueued = {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            if self.inner.shutdown.load(Ordering::Acquire) {
                false
            } else {
                state.liveness.push_back(job);
                true
            }
        };
        if enqueued {
            self.inner.signal.notify_one();
        }
        Handle { rx }
    }

    /// Submit a bulk op — throughput work no one is synchronously blocked on.
    pub fn submit_bulk<R, F>(&self, step: F, still_valid: Option<ValidityGuard>) -> Handle<R>
    where
        F: FnMut() -> ChunkOutcome<R> + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let job: Box<dyn BulkJob> = Box::new(BulkClosure {
            step,
            still_valid,
            progress: Arc::clone(&self.inner.progress),
            tx: Some(tx),
        });
        let enqueued = {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            if self.inner.shutdown.load(Ordering::Acquire) {
                false
            } else {
                state.bulk.push_back(job);
                true
            }
        };
        if enqueued {
            self.inner.signal.notify_one();
        }
        Handle { rx }
    }
}

impl Drop for WriterQueue {
    fn drop(&mut self) {
        {
            let _guard = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            self.inner.shutdown.store(true, Ordering::Release);
        }
        self.inner.signal.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn worker_loop(inner: &Arc<Inner>) {
    loop {
        let pick = {
            let mut state = inner.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if inner.shutdown.load(Ordering::Acquire) {
                    state.liveness.clear();
                    state.bulk.clear();
                    return;
                }
                if let Some(job) = state.liveness.pop_front() {
                    break Pick::Liveness(job);
                }
                if let Some(job) = state.bulk.pop_front() {
                    break Pick::Bulk(job);
                }
                state = inner.signal.wait(state).unwrap_or_else(|e| e.into_inner());
            }
        };
        inner.begin_work();
        match pick {
            Pick::Liveness(job) => job(true),
            Pick::Bulk(job) => run_bulk(inner, job),
        }
    }
}

fn run_bulk(inner: &Arc<Inner>, mut job: Box<dyn BulkJob>) {
    loop {
        inner.drain_liveness();

        if inner.shutdown.load(Ordering::Acquire) {
            job.deliver_dropped();
            return;
        }
        if !job.valid() {
            job.deliver_dropped();
            return;
        }
        let more = job.run_chunk();
        if !more {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    #[test]
    fn liveness_runs_fifo_and_returns_results() {
        let queue = WriterQueue::spawn("fifo-liveness");
        let order = Arc::new(Mutex::new(Vec::new()));

        let handles: Vec<_> = (0..3)
            .map(|i| {
                let order = Arc::clone(&order);
                queue.submit_liveness(move || {
                    order.lock().unwrap().push(i);
                    i * 10
                })
            })
            .collect();

        let results: Vec<_> = handles.into_iter().map(Handle::wait).collect();
        assert_eq!(
            results,
            vec![Outcome::Done(0), Outcome::Done(10), Outcome::Done(20)]
        );
        assert_eq!(*order.lock().unwrap(), vec![0, 1, 2]);
    }

    #[test]
    fn liveness_preempts_bulk_at_chunk_boundary() {
        let queue = WriterQueue::spawn("preempt");
        let log = Arc::new(Mutex::new(Vec::<&'static str>::new()));

        let (at_boundary_tx, at_boundary_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();

        let bulk_log = Arc::clone(&log);
        let mut chunk = 0;
        let bulk = queue.submit_bulk(
            move || {
                if chunk == 0 {
                    bulk_log.lock().unwrap().push("chunk-0");
                    at_boundary_tx.send(()).unwrap();
                    resume_rx.recv().unwrap();
                    chunk += 1;
                    ChunkOutcome::More
                } else {
                    bulk_log.lock().unwrap().push("chunk-1");
                    ChunkOutcome::Done("bulk-done")
                }
            },
            None,
        );

        at_boundary_rx.recv().unwrap();
        let live_log = Arc::clone(&log);
        let live = queue.submit_liveness(move || {
            live_log.lock().unwrap().push("liveness");
            7u8
        });
        resume_tx.send(()).unwrap();

        assert_eq!(live.wait(), Outcome::Done(7));
        assert_eq!(bulk.wait(), Outcome::Done("bulk-done"));
        assert_eq!(
            *log.lock().unwrap(),
            vec!["chunk-0", "liveness", "chunk-1"],
            "liveness must land between chunks, never splitting one"
        );
    }

    #[test]
    fn all_pending_liveness_runs_before_next_bulk_chunk() {
        let queue = WriterQueue::spawn("drain-all");
        let log = Arc::new(Mutex::new(Vec::<String>::new()));

        let (at_boundary_tx, at_boundary_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();

        let bulk_log = Arc::clone(&log);
        let mut chunk = 0;
        let bulk = queue.submit_bulk(
            move || {
                if chunk == 0 {
                    bulk_log.lock().unwrap().push("chunk-0".to_string());
                    at_boundary_tx.send(()).unwrap();
                    resume_rx.recv().unwrap();
                    chunk += 1;
                    ChunkOutcome::More
                } else {
                    bulk_log.lock().unwrap().push("chunk-1".to_string());
                    ChunkOutcome::Done(())
                }
            },
            None,
        );

        at_boundary_rx.recv().unwrap();
        let live_handles: Vec<_> = (0..3)
            .map(|i| {
                let live_log = Arc::clone(&log);
                queue
                    .submit_liveness(move || live_log.lock().unwrap().push(format!("liveness-{i}")))
            })
            .collect();
        resume_tx.send(()).unwrap();

        for handle in live_handles {
            assert_eq!(handle.wait(), Outcome::Done(()));
        }
        assert_eq!(bulk.wait(), Outcome::Done(()));
        assert_eq!(
            *log.lock().unwrap(),
            vec![
                "chunk-0".to_string(),
                "liveness-0".to_string(),
                "liveness-1".to_string(),
                "liveness-2".to_string(),
                "chunk-1".to_string(),
            ],
        );
    }

    #[test]
    fn bulk_ops_are_fifo_and_do_not_interleave() {
        let queue = WriterQueue::spawn("fifo-bulk");
        let log = Arc::new(Mutex::new(Vec::<&'static str>::new()));

        let make = |queue: &WriterQueue, first: &'static str, second: &'static str| {
            let log = Arc::clone(&log);
            let mut chunk = 0;
            queue.submit_bulk(
                move || {
                    if chunk == 0 {
                        log.lock().unwrap().push(first);
                        chunk += 1;
                        ChunkOutcome::More
                    } else {
                        log.lock().unwrap().push(second);
                        ChunkOutcome::Done(())
                    }
                },
                None,
            )
        };

        let first = make(&queue, "a0", "a1");
        let second = make(&queue, "b0", "b1");

        assert_eq!(first.wait(), Outcome::Done(()));
        assert_eq!(second.wait(), Outcome::Done(()));
        assert_eq!(*log.lock().unwrap(), vec!["a0", "a1", "b0", "b1"]);
    }

    #[test]
    fn stale_bulk_op_is_dropped_at_next_boundary() {
        let queue = WriterQueue::spawn("stale-drop");
        let log = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let valid = Arc::new(AtomicBool::new(true));

        let (at_boundary_tx, at_boundary_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();

        let bulk_log = Arc::clone(&log);
        let mut chunk = 0;
        let guard_flag = Arc::clone(&valid);
        let bulk = queue.submit_bulk(
            move || {
                if chunk == 0 {
                    bulk_log.lock().unwrap().push("chunk-0");
                    at_boundary_tx.send(()).unwrap();
                    resume_rx.recv().unwrap();
                    chunk += 1;
                    ChunkOutcome::More
                } else {
                    bulk_log.lock().unwrap().push("chunk-1");
                    ChunkOutcome::Done(())
                }
            },
            Some(Box::new(move || guard_flag.load(Ordering::Acquire))),
        );

        at_boundary_rx.recv().unwrap();
        valid.store(false, Ordering::Release);
        resume_tx.send(()).unwrap();

        assert_eq!(bulk.wait(), Outcome::Dropped);

        let follow = queue.submit_liveness(|| 99u8);
        assert_eq!(follow.wait(), Outcome::Done(99));

        assert_eq!(*log.lock().unwrap(), vec!["chunk-0"]);
    }

    #[test]
    fn panicking_op_is_isolated() {
        let queue = WriterQueue::spawn("panic");

        let panicked = queue.submit_liveness(|| -> u8 { panic!("intentional test panic") });
        assert_eq!(panicked.wait(), Outcome::Panicked);

        let follow = queue.submit_liveness(|| 5u8);
        assert_eq!(follow.wait(), Outcome::Done(5));
    }

    #[test]
    fn progress_observes_liveness_busy_and_terminal_transitions() {
        let queue = WriterQueue::spawn("progress-liveness");
        assert_eq!(queue.progress(), WriterProgress::default());

        let (running_tx, running_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let handle = queue.submit_liveness(move || {
            running_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });

        running_rx.recv().unwrap();
        let busy = queue.progress();
        assert!(busy.busy, "an executing liveness op must report busy");
        assert!(busy.sequence > 0, "work start must advance the sequence");

        release_tx.send(()).unwrap();
        assert_eq!(handle.wait(), Outcome::Done(()));
        let idle = queue.progress();
        assert!(!idle.busy, "terminal completion must publish idle");
        assert!(idle.sequence > busy.sequence);
    }

    #[test]
    fn shutdown_drops_queued_ops_without_hanging() {
        let queue = WriterQueue::spawn("shutdown");

        let (running_tx, running_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        let blocker = queue.submit_liveness(move || {
            running_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            "blocker-done"
        });

        let ran = Arc::new(AtomicUsize::new(0));
        let queued: Vec<_> = (0..3)
            .map(|_| {
                let ran = Arc::clone(&ran);
                queue.submit_liveness(move || {
                    ran.fetch_add(1, Ordering::SeqCst);
                    0u8
                })
            })
            .collect();

        running_rx.recv().unwrap();

        let (done_tx, done_rx) = mpsc::channel();
        let dropper = std::thread::spawn(move || {
            drop(queue);
            done_tx.send(()).unwrap();
        });

        release_tx.send(()).unwrap();

        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("dropping the queue must not hang");
        dropper.join().unwrap();

        assert_eq!(blocker.wait(), Outcome::Done("blocker-done"));
        for handle in queued {
            let outcome = handle.wait();
            assert!(
                outcome == Outcome::Dropped || outcome == Outcome::Done(0),
                "queued op resolved to {outcome:?}"
            );
        }
    }
}
