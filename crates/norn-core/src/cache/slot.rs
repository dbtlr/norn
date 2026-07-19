//! The owner-side cache slot: the per-vault handle the owner summons, holding
//! the writer queue and the current generation.
//!
//! This is the engine's owner-facing interface (ADR 0013 / 0017). It is NOT a
//! daemon: no sockets, no IPC, no request routing — those are `norn-owner`'s
//! job. The slot owns the generational cache state and the single writer thread;
//! the owner drives its async request pipeline around
//! [`serve_read`](VaultCacheSlot::serve_read) and the writer-queue submission
//! surface.
//!
//! # Phase-2 scope
//!
//! Config is fixed for the slot's lifetime: the db is created at summon under a
//! known config, and an index-relevant config change is a resummon in the
//! registered-vault model (ADR 0017), not an in-place reopen. The
//! [`IndexIdentity`] carried on each generation and the single-flight
//! [`ensure_current`](VaultCacheSlot::ensure_current) reopen seam are kept for
//! the phase-4 watcher authority to plug into.

use std::collections::BTreeSet;
use std::sync::{Arc, Condvar, Mutex};

use camino::{Utf8Path, Utf8PathBuf};

use crate::cache::error::CacheError;
use crate::cache::freshness::{Freshness, FreshnessProbe, StatSweepProbe};
use crate::cache::generation::{read_pool_cap, Generation, GrowParams, IndexIdentity, ReadPool};
use crate::cache::writer::{graph_fingerprint, increment_chunk_budget};
use crate::cache::writer_queue::{ChunkOutcome, Outcome, WriterQueue};
use crate::cache::Cache;
use crate::domain::GraphIndex;

/// The resolved vault config a cache slot is opened under.
#[derive(Debug, Clone, Default)]
pub struct CacheOpenConfig {
    pub alias_field: Option<String>,
    pub files_ignore: Vec<String>,
    pub index_set: BTreeSet<String>,
    pub index_set_hash: String,
}

impl CacheOpenConfig {
    fn index_identity(&self) -> IndexIdentity {
        IndexIdentity {
            index_set_hash: self.index_set_hash.clone(),
            alias_field: self.alias_field.clone(),
            ignore: self.files_ignore.clone(),
        }
    }
}

/// The slice of slot state a generation-open op touches, behind an `Arc` so an
/// op running `'static` on the writer thread can hold it.
struct SharedSlot {
    current: Mutex<Option<Arc<Generation>>>,
    next_number: Mutex<u64>,
}

/// A per-vault cache slot: the writer queue plus the current generation.
pub struct VaultCacheSlot {
    vault_root: Utf8PathBuf,
    db_path: Utf8PathBuf,
    config: CacheOpenConfig,
    shared: Arc<SharedSlot>,
    queue: WriterQueue,
    /// Test-only, per-slot count of refresh ops that actually executed on the
    /// writer thread. Per-slot (not a global) so parallel tests don't interfere.
    /// The coalescing test asserts N concurrent stale readers collapse to ONE
    /// refresh run (ADR 0013).
    #[cfg(test)]
    refresh_runs: Arc<std::sync::atomic::AtomicUsize>,
    /// Test-only count of refresh() calls that have completed ticket
    /// registration (submitted-or-joined) but not yet returned. Lets the
    /// coalescing test release its writer blocker only once all N callers have
    /// registered, so the collapse-to-one is deterministic, not timing-raced.
    #[cfg(test)]
    refresh_arrivals: Arc<std::sync::atomic::AtomicUsize>,
}

impl VaultCacheSlot {
    /// Summon the slot: create the cache db at `db_path`, run the one-shot full
    /// build (warm-up), seed generation 1, and spawn the writer queue.
    pub fn create(
        db_path: impl AsRef<Utf8Path>,
        vault_root: impl AsRef<Utf8Path>,
        config: CacheOpenConfig,
    ) -> Result<Self, CacheError> {
        let db_path = db_path.as_ref().to_path_buf();
        let vault_root = vault_root.as_ref().to_path_buf();

        let generation =
            open_generation(&db_path, &vault_root, &config, 1, /* build = */ true)?;

        let shared = Arc::new(SharedSlot {
            current: Mutex::new(Some(Arc::new(generation))),
            next_number: Mutex::new(2),
        });
        let queue = WriterQueue::spawn(vault_root.as_str());

        Ok(Self {
            vault_root,
            db_path,
            config,
            shared,
            queue,
            #[cfg(test)]
            refresh_runs: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            refresh_arrivals: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
    }

    /// Test-only: how many refresh ops have executed on the writer thread for
    /// this slot.
    #[cfg(test)]
    pub(crate) fn refresh_runs(&self) -> usize {
        self.refresh_runs.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Test-only: how many refresh() callers have completed registration.
    #[cfg(test)]
    pub(crate) fn refresh_arrivals(&self) -> usize {
        self.refresh_arrivals
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The current generation, opening one via the writer queue if none exists
    /// (single-flight: concurrent stale callers coalesce to one open).
    pub fn ensure_current(&self) -> Result<Arc<Generation>, CacheError> {
        {
            let current = self
                .shared
                .current
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if let Some(generation) = current.as_ref() {
                if generation.index_identity == self.config.index_identity() {
                    return Ok(Arc::clone(generation));
                }
            }
        }
        // Cold or index-relevant change: submit a single-flight open op.
        let shared = Arc::clone(&self.shared);
        let db_path = self.db_path.clone();
        let vault_root = self.vault_root.clone();
        let config = self.config.clone();
        let handle = self
            .queue
            .submit_liveness(move || open_or_adopt(&shared, &db_path, &vault_root, &config));
        match handle.wait() {
            Outcome::Done(result) => result,
            Outcome::Dropped => Err(CacheError::Sqlite(rusqlite::Error::QueryReturnedNoRows)),
            Outcome::Panicked => Err(CacheError::Sqlite(rusqlite::Error::QueryReturnedNoRows)),
        }
    }

    /// Serve one read: bind the current generation, run the request-boundary
    /// freshness probe, refresh through the coalesced liveness op when Stale, then
    /// run `read` against a checked-out read connection.
    pub fn serve_read<T>(
        &self,
        read: impl FnOnce(&Cache) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let generation = self.ensure_current()?;
        {
            let conn = generation.checkout_read();
            if let Freshness::Fresh = StatSweepProbe.probe(&self.vault_root, &conn)? {
                return read(&conn);
            }
        }
        // Stale → coalesced refresh, then serve on a freshly checked-out conn (WAL
        // makes the committed refresh visible across the connection boundary).
        self.refresh(&generation)?;
        let conn = generation.checkout_read();
        read(&conn)
    }

    /// Run the coalesced freshness refresh on `generation`: the first arriving
    /// requester submits a liveness op that refreshes through the write
    /// connection; later arrivals join the same ticket.
    pub fn refresh(&self, generation: &Arc<Generation>) -> Result<(), CacheError> {
        let (ticket, submit) = {
            let mut pending = generation
                .refresh_pending
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            match pending.as_ref() {
                Some(ticket) => (Arc::clone(ticket), false),
                None => {
                    let ticket = RefreshTicket::new();
                    *pending = Some(Arc::clone(&ticket));
                    (ticket, true)
                }
            }
        };

        // Registration (submit-or-join) is now complete; a test can observe that
        // this caller has committed to a ticket before it blocks on `wait`.
        #[cfg(test)]
        self.refresh_arrivals
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        if submit {
            let gen = Arc::clone(generation);
            let vault_root = self.vault_root.clone();
            let ticket_op = Arc::clone(&ticket);
            #[cfg(test)]
            let counter = Arc::clone(&self.refresh_runs);
            // The requester awaits the ticket, not this op's handle.
            let _handle = self.queue.submit_liveness(move || {
                #[cfg(test)]
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                run_refresh_op(&gen, &vault_root, &ticket_op)
            });
        }

        ticket.wait()
    }

    /// Commit a mutation's cache increment for `touched_paths` against `baseline`
    /// (the generation-N graph the mutation was planned over). Reserve on the
    /// write connection → build the driver off the writer thread → drive chunked
    /// TEMP staging and the terminal publish as a bulk op on the writer queue,
    /// fire-and-degrade.
    ///
    /// DORMANT in phase 2: the engine lands this whole seam (schema, staging,
    /// epoch supersession, the `data_version` guard) but the owner wires apply-time
    /// mutation to it in phase 3. `seq_alloc` is not engine scope.
    pub fn commit_apply_increments(
        &self,
        touched_paths: &[Utf8PathBuf],
        baseline: GraphIndex,
    ) -> Result<ApplyIncrementOutcome, CacheError> {
        let generation = self.ensure_current()?;
        let expected_fingerprint = graph_fingerprint(&baseline);

        // Reserve the publication baseline on the write connection.
        let reservation = {
            let mut write = generation
                .write_cache
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            write.reserve_increment_commit(&expected_fingerprint)?
        };

        // Build the driver off the writer thread (the parse is read-only over the
        // filesystem).
        let commit = match Cache::begin_increment_commit(
            &self.vault_root,
            touched_paths,
            self.config.alias_field.as_deref(),
            &self.config.files_ignore,
            &reservation,
            baseline,
        ) {
            Ok(commit) => commit,
            Err(error) => {
                let mut write = generation
                    .write_cache
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                let _ = write.discard_increment_reservation(&reservation);
                return Err(error);
            }
        };

        // Drive the chunks as a bulk op, preemptible by liveness at each boundary.
        let commit = Arc::new(Mutex::new(commit));
        let budget = increment_chunk_budget();
        let gen_step = Arc::clone(&generation);
        let commit_step = Arc::clone(&commit);
        // Drop-on-death guard: abandon the increment if its target generation is
        // no longer the slot's current one (reads `Generation::number`).
        let shared = Arc::clone(&self.shared);
        let number = generation.number;
        let handle = self.queue.submit_bulk(
            move || {
                let mut commit = commit_step.lock().unwrap_or_else(|p| p.into_inner());
                let mut write = gen_step
                    .write_cache
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                match write.commit_increment_chunk(&mut commit, budget) {
                    Ok(true) => ChunkOutcome::More,
                    Ok(false) => ChunkOutcome::Done(Ok(())),
                    Err(error) => ChunkOutcome::Done(Err(error.to_string())),
                }
            },
            Some(Box::new(move || {
                shared
                    .current
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .as_ref()
                    .is_some_and(|g| g.number == number)
            })),
        );

        match handle.wait() {
            Outcome::Done(Ok(())) => Ok(ApplyIncrementOutcome::Published),
            Outcome::Done(Err(message)) => Err(CacheError::Io {
                path: Utf8PathBuf::from("<increment-commit>"),
                source: std::io::Error::other(message),
            }),
            // The guard turned false or the queue shut down: the target generation
            // is gone, so the next generation's open re-derives everything.
            Outcome::Dropped | Outcome::Panicked => Ok(ApplyIncrementOutcome::Degraded),
        }
    }

    /// The writer queue, for submitting bulk mutation-increment commits (phase 3).
    pub fn queue(&self) -> &WriterQueue {
        &self.queue
    }

    /// A coherent snapshot of this slot's writer progress (ADR 0013 control
    /// plane) — the `{ busy, sequence }` the owner reports in a scoped pong.
    pub fn writer_progress(&self) -> crate::cache::WriterProgress {
        self.queue.progress_snapshot()
    }

    /// The vault root this slot serves.
    pub fn vault_root(&self) -> &Utf8Path {
        &self.vault_root
    }
}

/// The result of a [`commit_apply_increments`](VaultCacheSlot::commit_apply_increments) call.
#[derive(Debug, PartialEq, Eq)]
pub enum ApplyIncrementOutcome {
    /// The increment staged and published its relational snapshot.
    Published,
    /// The target generation died mid-commit (or the queue shut down); the
    /// increment was abandoned and the next generation re-derives the graph.
    Degraded,
}

/// A one-shot coalescing ticket a set of refresh requesters share (ADR 0013).
pub struct RefreshTicket {
    state: Mutex<Option<Result<(), String>>>,
    resolved: Condvar,
}

impl RefreshTicket {
    fn new() -> Arc<Self> {
        Arc::new(RefreshTicket {
            state: Mutex::new(None),
            resolved: Condvar::new(),
        })
    }

    fn resolve(&self, result: Result<(), CacheError>) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        *state = Some(result.map_err(|e| e.to_string()));
        self.resolved.notify_all();
    }

    fn wait(&self) -> Result<(), CacheError> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        while state.is_none() {
            state = self.resolved.wait(state).unwrap_or_else(|p| p.into_inner());
        }
        match state.as_ref().expect("resolved") {
            Ok(()) => Ok(()),
            // The first waiter took the concrete error; coalesced joiners
            // synthesize a same-signal error carrying its message.
            Err(message) => Err(CacheError::Io {
                path: Utf8PathBuf::from("<coalesced-refresh>"),
                source: std::io::Error::other(message.clone()),
            }),
        }
    }
}

/// The body of the coalesced refresh liveness op, run serialized on the writer
/// thread. Clears the pending ticket (so a later requester starts a new one),
/// runs the incremental refresh through the write connection, then supersedes any
/// staged increments the newer snapshot invalidated.
fn run_refresh_op(generation: &Generation, vault_root: &Utf8Path, ticket: &Arc<RefreshTicket>) {
    // Clear pending under the lock so a requester arriving after this point opens
    // a fresh ticket rather than joining one that is already committing.
    {
        let mut pending = generation
            .refresh_pending
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if pending
            .as_ref()
            .is_some_and(|pending| Arc::ptr_eq(pending, ticket))
        {
            *pending = None;
        }
    }

    let mut write_cache = generation
        .write_cache
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let result = write_cache.index_incremental(vault_root, &Default::default());
    if result.is_ok() {
        write_cache.supersede_staged_increments_after_refresh();
    }
    ticket.resolve(result.map(|_| ()));
}

/// Open generation N+1 (or adopt one an earlier op already produced) — the
/// single-flight coalescing seam, run serialized on the writer thread.
fn open_or_adopt(
    shared: &SharedSlot,
    db_path: &Utf8Path,
    vault_root: &Utf8Path,
    config: &CacheOpenConfig,
) -> Result<Arc<Generation>, CacheError> {
    {
        let current = shared.current.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(generation) = current.as_ref() {
            if generation.index_identity == config.index_identity() {
                return Ok(Arc::clone(generation));
            }
        }
    }
    let number = {
        let mut next = shared.next_number.lock().unwrap_or_else(|p| p.into_inner());
        let number = *next;
        *next += 1;
        number
    };
    // A reopen does not re-run the full build: the db content is already present.
    //
    // SEAM (phase 4, banked): this path is reached only on an index-relevant
    // config change, which is a resummon in the registered-vault model today, so
    // it does not fire in phase 2. Before it is wired live, an index-relevant
    // change MUST trigger a rebuild-or-reshred here (the deleted reshred-on-open) —
    // opening N+1 over a db built under the OLD index set would serve stale EAV
    // rows. Do not wire the watcher/config-change trigger to this without that.
    let generation = Arc::new(open_generation(
        db_path, vault_root, config, number, /* build = */ false,
    )?);
    *shared.current.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::clone(&generation));
    Ok(generation)
}

/// Open a generation: the primary read connection (optionally full-built),
/// the write connection, and the seeded read pool.
fn open_generation(
    db_path: &Utf8Path,
    vault_root: &Utf8Path,
    config: &CacheOpenConfig,
    number: u64,
    build: bool,
) -> Result<Generation, CacheError> {
    let mut primary = Cache::create(
        db_path,
        vault_root,
        config.alias_field.as_deref(),
        &config.files_ignore,
        config.index_set.clone(),
        &config.index_set_hash,
    )?;
    if build {
        primary.full_build(vault_root)?;
    }

    let write_cache = Cache::open_secondary(
        db_path,
        vault_root,
        config.alias_field.as_deref(),
        &config.files_ignore,
        config.index_set.clone(),
        &config.index_set_hash,
    )?;

    let grow = GrowParams {
        db_path: db_path.to_path_buf(),
        vault_root: vault_root.to_path_buf(),
        alias_field: config.alias_field.clone(),
        files_ignore: config.files_ignore.clone(),
        index_set: config.index_set.clone(),
        index_set_hash: config.index_set_hash.clone(),
    };
    let read_pool = ReadPool::seed(primary, grow, read_pool_cap())?;

    Ok(Generation {
        number,
        index_identity: config.index_identity(),
        read_pool,
        write_cache: Mutex::new(write_cache),
        refresh_pending: Mutex::new(None),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn vault() -> (TempDir, Utf8PathBuf, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let base = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let root = base.join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(root.join("a.md").as_std_path(), "---\ntype: note\n---\n").unwrap();
        std::fs::write(root.join("b.md").as_std_path(), "---\ntype: task\n---\n").unwrap();
        let db_path = base.join("cache.db");
        (tmp, root, db_path)
    }

    #[test]
    fn create_warms_up_and_serves_reads() {
        let (_tmp, root, db_path) = vault();
        let slot = VaultCacheSlot::create(&db_path, &root, CacheOpenConfig::default()).unwrap();
        let count = slot
            .serve_read(|cache| Ok(cache.documents_matching(&Default::default())?.len()))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn serve_read_refreshes_after_a_file_is_added() {
        let (_tmp, root, db_path) = vault();
        let slot = VaultCacheSlot::create(&db_path, &root, CacheOpenConfig::default()).unwrap();

        std::fs::write(root.join("c.md").as_std_path(), "---\ntype: note\n---\n").unwrap();

        let count = slot
            .serve_read(|cache| Ok(cache.documents_matching(&Default::default())?.len()))
            .unwrap();
        assert_eq!(count, 3, "a stale probe must route through the refresh");
    }

    #[test]
    fn commit_apply_increments_publishes_a_touched_document() {
        let (_tmp, root, db_path) = vault();
        let slot = VaultCacheSlot::create(&db_path, &root, CacheOpenConfig::default()).unwrap();

        // Baseline graph the mutation is planned over, then a new file lands.
        let baseline = slot.serve_read(|cache| cache.load_graph_index()).unwrap();
        std::fs::write(
            root.join("c.md").as_std_path(),
            "---\ntype: note\n---\nsee [[a]]\n",
        )
        .unwrap();

        let outcome = slot
            .commit_apply_increments(&[Utf8PathBuf::from("c.md")], baseline)
            .unwrap();
        assert_eq!(outcome, ApplyIncrementOutcome::Published);

        // The published increment is visible on the current generation's read
        // connection without a filesystem refresh.
        let generation = slot.ensure_current().unwrap();
        let conn = generation.checkout_read();
        let count = conn.documents_matching(&Default::default()).unwrap().len();
        assert_eq!(count, 3, "increment should have published c.md");
    }

    #[test]
    fn ensure_current_is_stable_across_calls() {
        let (_tmp, root, db_path) = vault();
        let slot = VaultCacheSlot::create(&db_path, &root, CacheOpenConfig::default()).unwrap();
        let g1 = slot.ensure_current().unwrap();
        let g2 = slot.ensure_current().unwrap();
        assert!(Arc::ptr_eq(&g1, &g2), "config unchanged → same generation");
    }

    // --- Deterministic concurrency properties over the writer-queue pipeline ---
    // (ADR 0013, re-derived from the donor's concurrency suite). These use the
    // writer-queue blocker pattern (a liveness op that parks the single writer
    // thread on a channel) plus per-slot observability counters — no
    // sleeps-as-synchronization; every wait is bounded on a condition.

    use std::sync::{mpsc, Barrier};
    use std::time::{Duration, Instant};

    /// Bounded spin until `cond` holds; panics if it never does within `budget`.
    fn spin_until(budget: Duration, mut cond: impl FnMut() -> bool) {
        let start = Instant::now();
        while !cond() {
            assert!(start.elapsed() < budget, "condition not met within {budget:?}");
            std::thread::yield_now();
        }
    }

    /// N concurrent stale readers collapse to exactly ONE refresh run. While the
    /// writer is parked on a blocker, the pending ticket never clears, so every
    /// refresh() that registers during the block joins that one op. We release
    /// the blocker only after all N have registered (observed via
    /// `refresh_arrivals`), so the collapse-to-one is deterministic.
    #[test]
    fn n_concurrent_refreshes_coalesce_to_one_run() {
        const N: usize = 8;
        let (_tmp, root, db_path) = vault();
        let slot =
            Arc::new(VaultCacheSlot::create(&db_path, &root, CacheOpenConfig::default()).unwrap());
        let generation = slot.ensure_current().unwrap();

        // Park the single writer thread so no refresh op can run yet.
        let (running_tx, running_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let blocker = slot.queue().submit_liveness(move || {
            running_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
        });
        running_rx.recv().unwrap(); // writer confirmed parked

        let barrier = Arc::new(Barrier::new(N + 1));
        let handles: Vec<_> = (0..N)
            .map(|_| {
                let slot = Arc::clone(&slot);
                let generation = Arc::clone(&generation);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    slot.refresh(&generation).unwrap();
                })
            })
            .collect();

        barrier.wait(); // release all N readers at once
        // All must register on the (single, pending) ticket before we let the
        // writer run — this is what makes the collapse deterministic.
        spin_until(Duration::from_secs(10), || slot.refresh_arrivals() == N);

        resume_tx.send(()).unwrap(); // let the one coalesced refresh run
        assert_eq!(blocker.wait(), Outcome::Done(()));
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(
            slot.refresh_runs(),
            1,
            "N concurrent stale reads must coalesce to exactly one refresh"
        );
    }

    /// N concurrent stale `serve_read`s all observe the refreshed vault — the
    /// coalesced-refresh path is correct under concurrency (no lost update, no
    /// deadlock, no panic), independent of the collapse count.
    #[test]
    fn concurrent_stale_serve_reads_all_see_the_refresh() {
        const N: usize = 8;
        let (_tmp, root, db_path) = vault(); // seeds a.md + b.md == 2 docs
        let slot =
            Arc::new(VaultCacheSlot::create(&db_path, &root, CacheOpenConfig::default()).unwrap());

        // Make the vault stale for every reader.
        std::fs::write(root.join("c.md").as_std_path(), "---\ntype: note\n---\n").unwrap();

        let barrier = Arc::new(Barrier::new(N));
        let handles: Vec<_> = (0..N)
            .map(|_| {
                let slot = Arc::clone(&slot);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    slot.serve_read(|c| Ok(c.documents_matching(&Default::default())?.len()))
                        .unwrap()
                })
            })
            .collect();

        for h in handles {
            assert_eq!(h.join().unwrap(), 3, "every concurrent reader must see c.md");
        }
    }

    /// Single-flight generation binding: N concurrent `ensure_current` calls all
    /// resolve to the SAME generation `Arc` — the slot never hands out two live
    /// generations for one config (the donor's single-flight-open property, as
    /// it manifests in the pre-seeded phase-2 slot; the index-change reopen path
    /// is banked/dormant per the module docs).
    #[test]
    fn concurrent_ensure_current_is_single_flight() {
        const N: usize = 12;
        let (_tmp, root, db_path) = vault();
        let slot =
            Arc::new(VaultCacheSlot::create(&db_path, &root, CacheOpenConfig::default()).unwrap());

        let barrier = Arc::new(Barrier::new(N));
        let handles: Vec<_> = (0..N)
            .map(|_| {
                let slot = Arc::clone(&slot);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    slot.ensure_current().unwrap()
                })
            })
            .collect();

        let gens: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let first = &gens[0];
        for g in &gens[1..] {
            assert!(
                Arc::ptr_eq(first, g),
                "all concurrent ensure_current calls must share one generation"
            );
        }
    }

    /// Liveness preempts bulk at chunk boundaries over the real slot pipeline: a
    /// bulk mutation-increment commit and concurrent liveness refreshes both
    /// complete without deadlock, and the published increment is visible. (The
    /// exact between-chunks interleave is pinned deterministically by the
    /// writer-queue unit tests; here we assert the slot-level coexistence.)
    #[test]
    fn bulk_commit_and_concurrent_liveness_both_complete() {
        let (_tmp, root, db_path) = vault();
        let slot =
            Arc::new(VaultCacheSlot::create(&db_path, &root, CacheOpenConfig::default()).unwrap());

        let baseline = slot.serve_read(|c| c.load_graph_index()).unwrap();
        std::fs::write(
            root.join("c.md").as_std_path(),
            "---\ntype: note\n---\nsee [[a]]\n",
        )
        .unwrap();

        // Fire a bulk increment commit and several liveness reads concurrently.
        let bulk_slot = Arc::clone(&slot);
        let bulk = std::thread::spawn(move || {
            bulk_slot.commit_apply_increments(&[Utf8PathBuf::from("c.md")], baseline)
        });

        let readers: Vec<_> = (0..4)
            .map(|_| {
                let slot = Arc::clone(&slot);
                std::thread::spawn(move || {
                    slot.serve_read(|c| Ok(c.documents_matching(&Default::default())?.len()))
                        .unwrap()
                })
            })
            .collect();

        assert_eq!(bulk.join().unwrap().unwrap(), ApplyIncrementOutcome::Published);
        for r in readers {
            let n = r.join().unwrap();
            assert!(n == 2 || n == 3, "reader saw an in-range count: {n}");
        }

        // The published increment is visible afterwards.
        let generation = slot.ensure_current().unwrap();
        let conn = generation.checkout_read();
        assert_eq!(conn.documents_matching(&Default::default()).unwrap().len(), 3);
    }
}
