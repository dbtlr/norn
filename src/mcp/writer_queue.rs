//! Per-vault writer queue — the single serialization point for all write-shaped
//! work against one vault (ADR 0013 Phase 2, NRN-252).
//!
//! # Why a queue at all
//!
//! Warm-mode `norn serve` holds one long-lived generation per vault (see
//! [`context`](super::context)). Everything that *writes* — opening the next
//! generation, running the incremental freshness refresh, committing a mutation
//! increment, and (later) draining a filesystem-watcher batch — touches blocking
//! rusqlite and the filesystem. Left unordered, two writers racing the same
//! connection deadlock or corrupt; serialized behind a coarse lock, a long bulk
//! mutation starves the cheap refresh a reader is blocked on. The queue resolves
//! both: **one** dedicated OS thread per vault owns all write work, and a
//! two-class schedule keeps latency-critical work ahead of throughput work.
//!
//! All three production consumers now route through it: generation opens and the
//! per-request freshness refresh (liveness), and the post-apply cache-increment
//! commit (bulk, NRN-252 / NRN-158).
//!
//! # Two submission classes
//!
//! - **Liveness** ([`WriterQueue::submit_liveness`]) — work a reader is *blocked
//!   on*: a freshness refresh, opening the generation a request needs. Runs to
//!   completion, FIFO among liveness ops, and **preempts bulk at the next chunk
//!   boundary**. A liveness op is a plain `FnOnce() -> R`.
//! - **Bulk** ([`WriterQueue::submit_bulk`]) — throughput work no one is
//!   synchronously waiting on: a mutation increment commit, a future watcher
//!   batch. Runs FIFO among bulk ops, yielding to liveness between chunks. A bulk
//!   op is a *chunked* op — a closure called repeatedly, once per chunk, each call
//!   returning [`ChunkOutcome::More`] or [`ChunkOutcome::Done`].
//!
//! # Chunk-boundary preemption
//!
//! The queue never interrupts a chunk mid-flight — a chunk is the atom of
//! progress and of preemption. Between chunks the writer drains the **entire**
//! liveness queue before resuming the bulk op, so a burst of refreshes all clear
//! ahead of the next chunk rather than one-per-boundary. Chunk *sizing* (the
//! ~50ms-per-chunk target that bounds preemption latency) is the op's concern and
//! lands with the op implementations in a later commit — the queue only decides
//! *when* to check for pending liveness work (after every chunk), never how big a
//! chunk is.
//!
//! # Drop-on-generation-death guard
//!
//! A bulk op may target a generation that dies before or during its run (an
//! out-of-band `cache clear`, a corruption bump — see `ensure_current` in
//! [`context`](super::context)). [`WriterQueue::submit_bulk`] takes an optional
//! `still_valid` predicate, checked before the first chunk and before every
//! subsequent chunk. When it turns false the op is **dropped without further
//! chunks** and its handle resolves to [`Outcome::Dropped`], distinct from a
//! completed result. This is the seam a later commit uses to abandon a
//! mutation aimed at a superseded generation instead of writing through a stale
//! connection.
//!
//! # Shutdown
//!
//! Dropping the [`WriterQueue`] handle shuts the thread down cleanly: it finishes
//! the current chunk (never interrupted mid-chunk), drops every still-queued op —
//! whose handles then resolve as [`Outcome::Dropped`] rather than hanging — and
//! exits. The `Drop` impl joins the thread, so there is never a detached,
//! forever-running writer.
//!
//! # Panic safety
//!
//! An op that panics must not take the queue down with it. Each op runs inside
//! [`catch_unwind`]; a panicking op resolves its own handle to
//! [`Outcome::Panicked`] and the writer keeps serving subsequent ops. The state
//! mutex is only ever held to move ops on and off the deques — never across op
//! execution — so an op panic can never poison it. This mirrors the generational
//! self-heal `WarmGuard::Drop` performs when a tool body panics while holding the
//! cache guard (see [`context`](super::context)).

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

/// A validity predicate for a bulk op, re-checked at every chunk boundary. Named
/// as an alias to keep the [`WriterQueue::submit_bulk`] signature legible.
pub(crate) type ValidityGuard = Box<dyn Fn() -> bool + Send>;

/// A type-erased liveness op: the caller's `FnOnce() -> R` wrapped so it captures
/// its own result sender and swallows nothing observable to the queue thread.
type LivenessJob = Box<dyn FnOnce() + Send>;

/// One step of a chunked bulk op.
///
/// The op returns [`More`](ChunkOutcome::More) while it has further work — the
/// queue will re-invoke it after draining any pending liveness — or
/// [`Done`](ChunkOutcome::Done) with the final result on the last chunk.
pub enum ChunkOutcome<R> {
    /// Further chunks remain; the queue re-invokes the op after a preemption check.
    More,
    /// The op finished; carries the result delivered to the submitter's handle.
    Done(R),
}

/// How a submitted op ultimately resolved, observed via [`Handle::wait`].
///
/// `Dropped` and `Panicked` are deliberately distinct from `Done`: a caller that
/// must know whether its write actually landed branches on the variant rather
/// than conflating "abandoned" or "crashed" with a real result.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome<R> {
    /// The op ran to completion; carries its return value.
    Done(R),
    /// The op was abandoned without completing — its `still_valid` guard turned
    /// false, or the queue shut down before (or while) it ran.
    Dropped,
    /// The op panicked; the queue caught the unwind and kept serving.
    Panicked,
}

/// A blocking handle to a submitted op's eventual [`Outcome`].
///
/// The async request path wraps [`wait`](Handle::wait) in `spawn_blocking`, so a
/// blocking receive is the right primitive: the caller parks a pool thread until
/// the writer resolves the op. A disconnected channel (the op was dropped on
/// shutdown without an explicit resolution) surfaces as [`Outcome::Dropped`]
/// rather than an error or a hang.
#[must_use = "a submitted op's outcome should be awaited via `wait`"]
pub struct Handle<R> {
    rx: mpsc::Receiver<Outcome<R>>,
}

impl<R> Handle<R> {
    /// Block until the op resolves, returning its [`Outcome`]. Never hangs past
    /// the op's completion and never panics: if the writer dropped the op without
    /// sending (queue shutdown), the disconnected channel resolves to
    /// [`Outcome::Dropped`].
    pub fn wait(self) -> Outcome<R> {
        self.rx.recv().unwrap_or(Outcome::Dropped)
    }
}

/// The work the writer thread serializes, guarded by [`Inner::state`].
struct State {
    /// Liveness ops, FIFO; fully drained between bulk chunks.
    liveness: VecDeque<LivenessJob>,
    /// Bulk ops, FIFO; run one at a time, chunk by chunk.
    bulk: VecDeque<Box<dyn BulkJob>>,
}

/// Shared between the [`WriterQueue`] handle and its writer thread.
struct Inner {
    state: Mutex<State>,
    /// Signalled on every submission and on shutdown to wake an idle writer.
    signal: Condvar,
    /// Set true (under the `state` lock, so the writer never misses the wakeup)
    /// when the handle drops. Read locklessly on the hot path.
    shutdown: AtomicBool,
}

impl Inner {
    /// Run every currently-queued liveness op to completion, FIFO. Ops submitted
    /// *while* draining are picked up too — the loop exits only on an empty queue —
    /// so a burst all clears before the caller resumes bulk work. Stops early if
    /// shutdown was requested, leaving the remainder for the shutdown drain to drop.
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
                Some(job) => job(),
                None => return,
            }
        }
    }
}

/// A type-erased chunked bulk op. The concrete implementor
/// ([`BulkClosure`]) owns the caller's chunk closure, the optional validity
/// guard, and the one-shot result sender.
trait BulkJob: Send {
    /// Whether the op should still run — checked before every chunk. A guard that
    /// panics counts as invalid (fail-closed).
    fn valid(&self) -> bool;
    /// Run one chunk. Returns `true` if more chunks remain, `false` if the op
    /// finished (or panicked); in the finished case the result has already been
    /// delivered to the handle.
    fn run_chunk(&mut self) -> bool;
    /// Resolve the handle as [`Outcome::Dropped`] — used when the validity guard
    /// fails or the queue shuts the op down before it completed.
    fn deliver_dropped(&mut self);
}

/// The concrete [`BulkJob`] backing a [`WriterQueue::submit_bulk`] call.
struct BulkClosure<R, F> {
    step: F,
    still_valid: Option<ValidityGuard>,
    /// `Some` until exactly one outcome is sent; taken to enforce single delivery.
    tx: Option<mpsc::Sender<Outcome<R>>>,
}

impl<R, F> BulkClosure<R, F> {
    /// Deliver the op's single outcome, if it has not been delivered already.
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
            // A panicking guard is treated as "no longer valid": fail-closed so a
            // buggy predicate abandons the op rather than forcing it through.
            Some(guard) => catch_unwind(AssertUnwindSafe(guard)).unwrap_or(false),
        }
    }

    fn run_chunk(&mut self) -> bool {
        let step = &mut self.step;
        match catch_unwind(AssertUnwindSafe(step)) {
            Ok(ChunkOutcome::More) => true,
            Ok(ChunkOutcome::Done(result)) => {
                self.resolve(Outcome::Done(result));
                false
            }
            Err(_) => {
                self.resolve(Outcome::Panicked);
                false
            }
        }
    }

    fn deliver_dropped(&mut self) {
        self.resolve(Outcome::Dropped);
    }
}

/// What the writer picked to run next off the deques.
enum Pick {
    Liveness(LivenessJob),
    Bulk(Box<dyn BulkJob>),
}

/// A per-vault writer queue owning one dedicated OS thread. See the module docs
/// for the two-class model, chunk-boundary preemption, the drop-on-death guard,
/// and shutdown semantics (ADR 0013, NRN-252).
///
/// Write work is blocking rusqlite / filesystem work, so this is a `std::thread`,
/// **not** a tokio task — the async request path bridges to it via `spawn_blocking`
/// around [`Handle::wait`].
pub struct WriterQueue {
    inner: Arc<Inner>,
    /// `Some` until `Drop` joins the thread. `Option` only so `Drop` can take it.
    worker: Option<JoinHandle<()>>,
}

impl WriterQueue {
    /// Spawn the queue and its writer thread. `name` labels the OS thread only
    /// (e.g. the vault root) for debuggability; it has no semantic effect.
    pub fn spawn(name: &str) -> WriterQueue {
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                liveness: VecDeque::new(),
                bulk: VecDeque::new(),
            }),
            signal: Condvar::new(),
            shutdown: AtomicBool::new(false),
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

    /// Submit a liveness op — latency-critical work a reader is blocked on. Runs
    /// FIFO among liveness ops and preempts any in-flight bulk op at the next
    /// chunk boundary. The returned [`Handle`] resolves to the op's result, or to
    /// [`Outcome::Panicked`] / [`Outcome::Dropped`] (the latter if the queue was
    /// already shutting down).
    pub fn submit_liveness<R, F>(&self, op: F) -> Handle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let job: LivenessJob = Box::new(move || {
            let outcome = match catch_unwind(AssertUnwindSafe(op)) {
                Ok(result) => Outcome::Done(result),
                Err(_) => Outcome::Panicked,
            };
            let _ = tx.send(outcome);
        });
        // A job racing shutdown is dropped rather than enqueued: its sender then
        // disconnects and the handle resolves to `Dropped` instead of hanging.
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

    /// Submit a bulk op — throughput work no one is synchronously blocked on. The
    /// op is a chunked closure invoked once per chunk; between chunks the queue
    /// drains all pending liveness. `still_valid`, if given, is checked before the
    /// first chunk and before every subsequent chunk; when it turns false the op
    /// is dropped and its [`Handle`] resolves to [`Outcome::Dropped`].
    pub fn submit_bulk<R, F>(&self, step: F, still_valid: Option<ValidityGuard>) -> Handle<R>
    where
        F: FnMut() -> ChunkOutcome<R> + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let job: Box<dyn BulkJob> = Box::new(BulkClosure {
            step,
            still_valid,
            tx: Some(tx),
        });
        // See `submit_liveness`: a job racing shutdown is dropped, not enqueued.
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

#[cfg(test)]
impl WriterQueue {
    /// Test-only cross-module shutdown observer. Hands back a cheap handle that
    /// holds its own `Arc` to the shared state, so it stays valid after the
    /// `WriterQueue` (and whatever owns it) has been dropped — letting a test
    /// release a writer-occupying op only once shutdown has been committed, making
    /// a queued op's drop deterministic.
    pub(crate) fn shutdown_watch(&self) -> ShutdownWatch {
        ShutdownWatch(Arc::clone(&self.inner))
    }
}

/// Test-only opaque observer over a queue's shutdown flag (see
/// [`WriterQueue::shutdown_watch`]).
#[cfg(test)]
pub(crate) struct ShutdownWatch(Arc<Inner>);

#[cfg(test)]
impl ShutdownWatch {
    /// Has the owning [`WriterQueue`] begun (or completed) shutting down?
    pub(crate) fn is_shutting_down(&self) -> bool {
        self.0.shutdown.load(Ordering::Acquire)
    }
}

impl Drop for WriterQueue {
    /// Signal shutdown, wake the writer, and join it — finishing the in-flight
    /// chunk, dropping every queued op, and leaving no detached thread.
    fn drop(&mut self) {
        {
            // Set under the lock so a writer about to `wait` on the condvar
            // observes it and cannot miss the notify below (lost-wakeup safety).
            let _guard = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            self.inner.shutdown.store(true, Ordering::Release);
        }
        self.inner.signal.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// The writer thread's main loop: pick the next unit (liveness before bulk),
/// run it, repeat; on shutdown drop all queued ops and exit.
fn worker_loop(inner: &Arc<Inner>) {
    loop {
        let pick = {
            let mut state = inner.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if inner.shutdown.load(Ordering::Acquire) {
                    // Drop every queued op; their senders disconnect and the
                    // waiting handles resolve to `Outcome::Dropped`.
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
        match pick {
            Pick::Liveness(job) => job(),
            Pick::Bulk(job) => run_bulk(inner, job),
        }
    }
}

/// Run one bulk op to completion (or to a drop), yielding to liveness between
/// chunks. Before every chunk: drain all pending liveness, then honor shutdown
/// and the validity guard. The op is never interrupted mid-chunk.
fn run_bulk(inner: &Arc<Inner>, mut job: Box<dyn BulkJob>) {
    loop {
        // Preempt: liveness ahead of this bulk op, before the first chunk and
        // between every pair of chunks.
        inner.drain_liveness();

        if inner.shutdown.load(Ordering::Acquire) {
            job.deliver_dropped();
            return;
        }
        if !job.valid() {
            job.deliver_dropped();
            return;
        }
        if !job.run_chunk() {
            // Finished (result delivered) or panicked (handle already resolved).
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    /// A liveness op returns its result and ops run FIFO.
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

    /// A liveness op queued mid-bulk runs at the next chunk boundary — its log
    /// entry lands between two chunk entries, never splitting a chunk — and the
    /// bulk op still completes with its result.
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
                    // Announce we are paused mid-bulk, then block until the test
                    // has queued the liveness op — no sleeps, pure handshake.
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

    /// Every liveness op queued during a single bulk chunk runs before the next
    /// bulk chunk.
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

    /// Bulk ops are FIFO relative to one another and never interleave — the first
    /// op's chunks all precede the second op's chunks.
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

    /// `still_valid` turning false mid-op drops the bulk op at the next boundary;
    /// the submitter observes `Dropped`, the remaining chunk never runs, and a
    /// subsequent op still runs.
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
                    // Must never run: the guard flips false before this boundary.
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

        // The queue keeps serving after a dropped op.
        let follow = queue.submit_liveness(|| 99u8);
        assert_eq!(follow.wait(), Outcome::Done(99));

        assert_eq!(*log.lock().unwrap(), vec!["chunk-0"]);
    }

    /// Dropping the queue handle resolves queued-but-unstarted ops as `Dropped`
    /// (never running them), does not hang, and joins the thread cleanly.
    #[test]
    fn shutdown_drops_queued_ops_without_hanging() {
        let queue = WriterQueue::spawn("shutdown");
        let probe = queue.shutdown_watch();

        let (running_tx, running_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        // A first op occupies the writer thread so the rest stay queued.
        let blocker = queue.submit_liveness(move || {
            running_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            "blocker-done"
        });

        // These three never get to run before shutdown.
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

        // Drop the queue from another thread — it sets shutdown, then blocks in
        // join until the blocker (still running) returns.
        let (done_tx, done_rx) = mpsc::channel();
        let dropper = std::thread::spawn(move || {
            drop(queue);
            done_tx.send(()).unwrap();
        });

        // Wait until shutdown is committed, THEN release the blocker, so the
        // writer's next pick observes shutdown and drops the queued ops rather
        // than running them. Guaranteed to terminate: the dropper sets it.
        while !probe.is_shutting_down() {
            std::thread::yield_now();
        }
        release_tx.send(()).unwrap();

        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("dropping the queue must not hang");
        dropper.join().unwrap();

        assert_eq!(blocker.wait(), Outcome::Done("blocker-done"));
        for handle in queued {
            assert_eq!(handle.wait(), Outcome::Dropped);
        }
        assert_eq!(
            ran.load(Ordering::SeqCst),
            0,
            "queued ops must not run after shutdown"
        );
    }

    /// A panicking op resolves its handle to `Panicked`; the queue keeps serving.
    #[test]
    fn panicking_op_is_isolated() {
        let queue = WriterQueue::spawn("panic");

        let panicked = queue.submit_liveness(|| -> u8 { panic!("intentional test panic") });
        assert_eq!(panicked.wait(), Outcome::Panicked);

        let follow = queue.submit_liveness(|| 5u8);
        assert_eq!(follow.wait(), Outcome::Done(5));
    }
}
