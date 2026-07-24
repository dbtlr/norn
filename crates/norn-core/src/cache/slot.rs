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
    /// Derive the cache-open config from a parsed [`VaultConfig`](crate::standards::VaultConfig):
    /// the alias field, the file-ignore globs, and the resolved index set (plus
    /// its stable identity hash). Pure — no IO; the caller injects the parsed
    /// config (the owner reads `.norn/config.yaml` off disk, keeping norn-core
    /// IO-free). This is the single mapping from a vault's declared standards to
    /// the four knobs the cache engine accepts.
    ///
    /// The alias field is the fixed `aliases` convention (NRN-455), NOT read from
    /// `config.links.alias_field` — that key is retired and inert (see
    /// [`LinksConfig`](crate::standards::LinksConfig)). It drives only the derived
    /// `Document::aliases` set feeding `repair`'s alias-hint; aliases no longer
    /// participate in resolution or validation.
    pub fn from_vault_config(config: &crate::standards::VaultConfig) -> Self {
        let (index_set, index_set_hash) = crate::standards::resolved_index_set(config);
        Self {
            alias_field: Some(crate::graph::ALIAS_FRONTMATTER_FIELD.to_string()),
            files_ignore: config.files.ignore.clone(),
            index_set,
            index_set_hash,
        }
    }

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
        self.commit_apply_increments_tracked(touched_paths, baseline)
            .1
    }

    /// The body of [`commit_apply_increments`](Self::commit_apply_increments),
    /// also returning the generation number the increment was BOUND to — the one
    /// this call's own [`ensure_current`](Self::ensure_current) resolved, `None`
    /// when that resolution itself failed (no generation ran).
    ///
    /// The bound number is captured HERE, inside the commit's own scope, so a
    /// corruption-eviction targets exactly the generation the increment ran on.
    /// Capturing it in the caller (before this method's `ensure_current`) would be
    /// wrong: a concurrent reopen can advance the slot's current generation
    /// between the two points, and evicting the pre-captured (older) number would
    /// leave the generation the increment actually corrupted in place — the
    /// NRN-253 lag-hole, closed by binding the eviction key at the commit
    /// site.
    fn commit_apply_increments_tracked(
        &self,
        touched_paths: &[Utf8PathBuf],
        baseline: GraphIndex,
    ) -> (Option<u64>, Result<ApplyIncrementOutcome, CacheError>) {
        let generation = match self.ensure_current() {
            Ok(generation) => generation,
            Err(error) => return (None, Err(error)),
        };
        // The generation the increment is bound to — the eviction key, captured
        // from THIS call's `ensure_current`, not from the caller.
        let bound = generation.number;
        let expected_fingerprint = graph_fingerprint(&baseline);

        // Reserve the publication baseline on the write connection.
        let reservation = {
            let mut write = generation
                .write_cache
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            match write.reserve_increment_commit(&expected_fingerprint) {
                Ok(reservation) => reservation,
                Err(error) => return (Some(bound), Err(error)),
            }
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
                return (Some(bound), Err(error));
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
        let number = bound;
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

        let outcome = match handle.wait() {
            Outcome::Done(Ok(())) => Ok(ApplyIncrementOutcome::Published),
            Outcome::Done(Err(message)) => Err(CacheError::Io {
                path: Utf8PathBuf::from("<increment-commit>"),
                source: std::io::Error::other(message),
            }),
            // The guard turned false or the queue shut down: the target generation
            // is gone, so the next generation's open re-derives everything.
            Outcome::Dropped | Outcome::Panicked => Ok(ApplyIncrementOutcome::Degraded),
        };
        (Some(bound), outcome)
    }

    /// Commit a mutation's cache increment as **fire-and-degrade**: the mutation
    /// already succeeded on disk (files are the source of truth), so a failed
    /// increment MUST NOT surface as an error — it degrades to an operator note
    /// and the next read heals the cache off the filesystem.
    ///
    /// This is the caller-side adapter the HARD CONTRACT (NRN-386) requires:
    /// [`commit_apply_increments`](Self::commit_apply_increments) propagates a
    /// failed chunk commit as `Err(CacheError::Io { path: "<increment-commit>" })`
    /// and a stale baseline as `Err(CacheError::IncrementBaselineDrift)`. Mapping
    /// EITHER to a hard mutation failure would re-open the corrupt-generation hole
    /// (NRN-253): a generation whose staged increment failed would keep serving.
    /// So on ANY increment failure we EVICT the generation the increment ran on
    /// (dropping it forces the next [`ensure_current`](Self::ensure_current) to
    /// re-derive from files), surface an operator note the owner renders later,
    /// and SUCCEED. `Degraded` (the target generation died mid-commit) is already
    /// benign — no eviction, just the note.
    ///
    /// The eviction key is the generation
    /// [`commit_apply_increments_tracked`](Self::commit_apply_increments_tracked)
    /// BOUND (its own `ensure_current`), never a value captured here before the
    /// commit ran — closing the concurrency window where a reopen advances the
    /// slot between capture and bind and eviction would miss the corrupt generation.
    ///
    /// This evicts on ANY increment failure (not only the corruption class of a
    /// failed chunk commit). Rationale: it is trust-conservative (files are the
    /// source of truth, so a re-derive is always safe) and strictly closes the
    /// corrupt-generation-keeps-serving hole (NRN-253) rather than reasoning
    /// per-error about which failures can leave a generation serving stale rows;
    /// the cost is a spurious full re-derive on the benign baseline-drift path
    /// (rare — an increment failure at all is rare).
    pub fn commit_apply_increments_fire_and_degrade(
        &self,
        touched_paths: &[Utf8PathBuf],
        baseline: GraphIndex,
    ) -> ApplyIncrementCommit {
        let (bound, result) = self.commit_apply_increments_tracked(touched_paths, baseline);
        match result {
            Ok(ApplyIncrementOutcome::Published) => ApplyIncrementCommit::Published,
            Ok(ApplyIncrementOutcome::Degraded) => ApplyIncrementCommit::Degraded {
                operator_note:
                    "cache increment did not publish (generation superseded); the next read \
                     rebuilds it from the vault"
                        .to_string(),
            },
            Err(error) => {
                // Evict the generation the increment BOUND (returned by the commit),
                // not a value captured before it ran.
                self.evict_generation_on_corruption(bound);
                ApplyIncrementCommit::Degraded {
                    operator_note: format!(
                        "cache increment failed ({error}); the generation was evicted and the \
                         next read rebuilds it from the vault"
                    ),
                }
            }
        }
    }

    /// Evict the generation an increment ran on so the next open re-derives from
    /// files (NRN-253 corrupt-generation guard). Only drops the CURRENT generation
    /// when it is still the one the increment ran on (`ran_on`); a newer generation
    /// a concurrent open already installed is left intact. `ran_on == None` (the
    /// increment opened the slot from cold) evicts whatever it just installed.
    fn evict_generation_on_corruption(&self, ran_on: Option<u64>) {
        let mut current = self
            .shared
            .current
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let still_the_ran_on_generation = match (current.as_ref(), ran_on) {
            (Some(generation), Some(number)) => generation.number == number,
            (Some(_), None) => true,
            (None, _) => false,
        };
        if still_the_ran_on_generation {
            *current = None;
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

/// The result of the fire-and-degrade increment commit
/// ([`commit_apply_increments_fire_and_degrade`](VaultCacheSlot::commit_apply_increments_fire_and_degrade)).
///
/// The mutation always succeeds; this only reports whether the cache stayed warm.
/// `Degraded` carries an operator note the owner layer renders alongside the
/// `ApplyReport` (the cache did not update; the next read heals it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyIncrementCommit {
    /// The increment staged and published; the cache is warm.
    Published,
    /// The increment did not publish; the note explains why and the next read
    /// rebuilds the cache from the vault.
    Degraded { operator_note: String },
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
    fn fire_and_degrade_evicts_and_succeeds_when_the_increment_fails() {
        // HARD CONTRACT (NRN-386): a failed cache increment must NOT fail the
        // mutation. A drifted baseline (fingerprint mismatch vs the db) makes
        // `commit_apply_increments` return Err; the fire-and-degrade adapter maps
        // it to eviction + an operator note + success.
        let (_tmp, root, db_path) = vault();
        let slot = VaultCacheSlot::create(&db_path, &root, CacheOpenConfig::default()).unwrap();

        // Warm the slot and record the generation the increment will run on.
        let ran_on = slot.ensure_current().unwrap().number;

        // A baseline whose content (and thus fingerprint) does not match the db,
        // forcing `reserve_increment_commit` to reject it as baseline drift.
        let mut drifted = slot.serve_read(|cache| cache.load_graph_index()).unwrap();
        drifted.documents.clear();

        let commit =
            slot.commit_apply_increments_fire_and_degrade(&[Utf8PathBuf::from("a.md")], drifted);

        match commit {
            ApplyIncrementCommit::Degraded { operator_note } => {
                assert!(
                    operator_note.contains("evicted") || operator_note.contains("rebuilds"),
                    "the operator note explains the degrade: {operator_note}"
                );
            }
            ApplyIncrementCommit::Published => {
                panic!("a drifted baseline must degrade, not publish")
            }
        }

        // The corrupt generation was evicted: the next open re-derives a fresh
        // one, so the number advances by EXACTLY one. A skipped eviction (the
        // lag-hole: eviction keyed on a stale/wrong number would find
        // `current != ran_on` and no-op) would leave `healed == ran_on` — the
        // strict `+ 1` catches that, where a loose `>` would not.
        let healed = slot.ensure_current().unwrap().number;
        assert_eq!(
            healed,
            ran_on + 1,
            "eviction fired on the bound generation and reopened exactly once"
        );
        // And the vault is still fully readable — files are the source of truth.
        let count = slot
            .serve_read(|cache| Ok(cache.documents_matching(&Default::default())?.len()))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn fire_and_degrade_eviction_key_is_the_commit_bound_generation() {
        // F2 (NRN-386): the eviction key must be the generation the commit's OWN
        // `ensure_current` bound, not a value captured before the commit ran — the
        // guard against a concurrent reopen advancing the slot between capture and
        // bind. Pin it at the source: the tracked commit returns the number it
        // bound, and it equals the generation current at the commit site.
        let (_tmp, root, db_path) = vault();
        let slot = VaultCacheSlot::create(&db_path, &root, CacheOpenConfig::default()).unwrap();

        let current = slot.ensure_current().unwrap().number;
        let mut drifted = slot.serve_read(|cache| cache.load_graph_index()).unwrap();
        drifted.documents.clear();

        let (bound, result) =
            slot.commit_apply_increments_tracked(&[Utf8PathBuf::from("a.md")], drifted);
        assert!(result.is_err(), "a drifted baseline must fail the commit");
        assert_eq!(
            bound,
            Some(current),
            "the eviction key is the generation the commit's own ensure_current bound"
        );
    }

    #[test]
    fn fire_and_degrade_publishes_on_a_clean_increment() {
        let (_tmp, root, db_path) = vault();
        let slot = VaultCacheSlot::create(&db_path, &root, CacheOpenConfig::default()).unwrap();
        let baseline = slot.serve_read(|cache| cache.load_graph_index()).unwrap();
        std::fs::write(
            root.join("c.md").as_std_path(),
            "---\ntype: note\n---\nsee [[a]]\n",
        )
        .unwrap();
        let commit =
            slot.commit_apply_increments_fire_and_degrade(&[Utf8PathBuf::from("c.md")], baseline);
        assert_eq!(commit, ApplyIncrementCommit::Published);
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
    // (ADR 0013). These use the
    // writer-queue blocker pattern (a liveness op that parks the single writer
    // thread on a channel) plus per-slot observability counters — no
    // sleeps-as-synchronization; every wait is bounded on a condition.

    use std::sync::{mpsc, Barrier};
    use std::time::{Duration, Instant};

    /// Bounded spin until `cond` holds; panics if it never does within `budget`.
    fn spin_until(budget: Duration, mut cond: impl FnMut() -> bool) {
        let start = Instant::now();
        while !cond() {
            assert!(
                start.elapsed() < budget,
                "condition not met within {budget:?}"
            );
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
            assert_eq!(
                h.join().unwrap(),
                3,
                "every concurrent reader must see c.md"
            );
        }
    }

    /// Single-flight generation binding: N concurrent `ensure_current` calls all
    /// resolve to the SAME generation `Arc` — the slot never hands out two live
    /// generations for one config (the single-flight-open property, as
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
    /// bulk mutation-increment commit and concurrent liveness refreshes coexist —
    /// both complete without deadlock or panic, no reader observes a torn count,
    /// and the vault ends consistent with `c.md` present. (The exact between-chunks
    /// interleave is pinned deterministically by the writer-queue unit tests; here
    /// we assert the slot-level coexistence.)
    ///
    /// The bulk arm is deliberately NON-strict about *its own* success: the setup
    /// makes the vault stale for the concurrent readers (c.md is added before the
    /// threads fan out), so every reader routes through a coalesced refresh whose
    /// `index_incremental` advances the stored graph fingerprint on the shared
    /// write connection. When a refresh wins the `write_cache` lock before the
    /// bulk's `reserve_increment_commit`, the reserve legitimately observes
    /// `IncrementBaselineDrift` — the exact race the production caller absorbs via
    /// [`commit_apply_increments_fire_and_degrade`](VaultCacheSlot::commit_apply_increments_fire_and_degrade)
    /// (files are the source of truth; the winning refresh already carried c.md
    /// into the db). Asserting the raw commit MUST publish encodes an invalid
    /// assumption — that the increment always wins that race — and is what made
    /// this test flaky on higher-contention (CI) schedulers. So both
    /// `Published` and `IncrementBaselineDrift` are accepted; what stays strict is
    /// what the test actually proves: liveness, no deadlock/panic, and final
    /// consistency (c.md present either way, no lost update).
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

        // Both operations complete without deadlock or panic. The bulk arm either
        // publishes its own increment or observes a concurrent refresh having
        // advanced the baseline (IncrementBaselineDrift) — the production
        // fire-and-degrade contract. Any OTHER error would be a real defect.
        match bulk.join().expect("bulk thread must not panic") {
            Ok(ApplyIncrementOutcome::Published) | Err(CacheError::IncrementBaselineDrift) => {}
            other => {
                panic!("bulk arm must publish or drift on a concurrent refresh, got: {other:?}")
            }
        }
        for r in readers {
            let n = r.join().unwrap();
            assert!(n == 2 || n == 3, "reader saw an in-range count: {n}");
        }

        // Final consistency (no lost update): whichever arm won the race, c.md is
        // present — the bulk increment published it, or the refresh that drifted
        // the reserve already carried it into the db.
        let generation = slot.ensure_current().unwrap();
        let conn = generation.checkout_read();
        assert_eq!(
            conn.documents_matching(&Default::default()).unwrap().len(),
            3,
            "the vault ends consistent with c.md present, regardless of which arm won"
        );
    }
}
