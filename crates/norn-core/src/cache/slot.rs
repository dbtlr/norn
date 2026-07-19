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
        })
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

        if submit {
            let gen = Arc::clone(generation);
            let vault_root = self.vault_root.clone();
            let ticket_op = Arc::clone(&ticket);
            // The requester awaits the ticket, not this op's handle.
            let _handle = self
                .queue
                .submit_liveness(move || run_refresh_op(&gen, &vault_root, &ticket_op));
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
}
