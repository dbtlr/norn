//! Generation single-flight open / `ensure_current` and cache-dir setup.

use super::*;

/// `(dev, ino)` of a filesystem object. On unix this uniquely identifies the
/// underlying file across unlink+recreate; a swapped inode is exactly the
/// ground-shift signal warm mode watches for.
#[cfg(unix)]
pub(in crate::env) fn device_inode(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

/// Non-unix fallback: no stable `(dev, ino)`, so approximate with
/// `(len, mtime-secs)` as a best-effort change signal. norn's release targets
/// are all unix, so this path is never exercised in CI or releases; it exists
/// only to keep the crate compiling on non-unix hosts.
#[cfg(not(unix))]
pub(in crate::env) fn device_inode(meta: &std::fs::Metadata) -> (u64, u64) {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    (meta.len(), mtime)
}

/// The identity `(dev, ino)` of the live `<cache_dir>/cache.db` for `vault_root`,
/// or `None` when the file (or the cache dir) is absent / unstatable. Used by the
/// ground-shift check to compare against the identity captured at open.
pub(in crate::env) fn current_db_identity(vault_root: &Utf8Path) -> Option<(u64, u64)> {
    let (_canonical, cache_dir) = cache_dir_for(vault_root).ok()?;
    let db_path = cache_dir.join("cache.db");
    let meta = std::fs::metadata(db_path.as_std_path()).ok()?;
    Some(device_inode(&meta))
}

/// Is `generation` still fresh for the current world? False when it has been
/// invalidated (corruption / panic bumped the floor past it), when its `cache.db`
/// identity no longer matches the live file (ground-shift), or when the live
/// config's index identity differs from the one it was opened under (an
/// index-relevant config change).
///
/// A free function (not a `&self` method) so the writer-queue open op, which runs
/// `'static` on the writer thread, can call it with the same inputs the
/// per-request fast path uses (NRN-252).
pub(in crate::env) fn generation_is_fresh(
    generation: &Generation,
    floor: &AtomicU64,
    vault_root: &Utf8Path,
    config: &LoadedConfig,
) -> bool {
    generation.number >= floor.load(Ordering::Acquire)
        && current_db_identity(vault_root) == Some(generation.db_identity)
        && generation.index_identity.matches_config(config)
}

/// A `still_valid` guard for the increment-commit bulk op: the target generation
/// (identified by `number`) is STILL the slot's current generation and at/above
/// the invalidation floor (NRN-252). Re-checked at every chunk boundary; when it
/// turns false — an out-of-band `cache clear`, a corruption/panic floor bump, or
/// a newer generation swapped in — the remaining chunks are dropped without
/// running, because the next generation's open-scan re-derives everything and a
/// dead generation's write connection must not be touched further.
pub(in crate::env) fn generation_still_current_guard(
    shared: &Arc<SharedSlot>,
    number: u64,
) -> ValidityGuard {
    let shared = Arc::clone(shared);
    Box::new(move || {
        number >= shared.floor.load(Ordering::Acquire)
            && shared
                .current
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
                .is_some_and(|g| g.number == number)
    })
}

/// The body of a generation-open liveness op, run serialized on the writer thread.
///
/// It re-checks the current generation's freshness FIRST — the coalescing seam:
/// N concurrent stale callers each submit an op, the first opens N+1 and swaps the
/// `current` pointer, and the remaining ops find `current` fresh and adopt it,
/// so the counter advances by one, not N (NRN-252). Only if still stale does it
/// open generation N+1. This is the ONLY place `integrity_check` is paid in warm
/// mode; a stable connection is reused across requests.
///
/// Runs `'static`, so it takes owned / `Arc` state rather than `&self` / `&slot`.
pub(in crate::env) fn open_or_adopt(
    shared: &SharedSlot,
    vault_root: &Utf8Path,
    config: &LoadedConfig,
) -> Result<Arc<Generation>> {
    // Late-arrival adoption: now that we are serialized behind the queue, a
    // generation an earlier op opened may already satisfy us.
    {
        let snapshot = shared
            .current
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(generation) = &snapshot {
            if generation_is_fresh(generation, &shared.floor, vault_root, config) {
                return Ok(Arc::clone(generation));
            }
        }
    }

    // Still stale — open generation N+1 and swap it in.
    let number = {
        let mut next = shared.next_number.lock().unwrap_or_else(|p| p.into_inner());
        let number = *next;
        *next += 1;
        number
    };
    let generation = Arc::new(open_generation(vault_root, config, number)?);
    shared.open_count.fetch_add(1, Ordering::Relaxed);
    *shared.current.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::clone(&generation));
    Ok(generation)
}

/// Map a generation-open op's [`Outcome`] to a `Result`. A dropped op (the queue
/// was shutting down) or a panicked op becomes a descriptive error rather than a
/// hang or an `unwrap` — an `ensure_current` caller always gets a value or an
/// error, never a wedged request (NRN-252).
pub(in crate::env) fn map_open_outcome(
    outcome: Outcome<Result<Arc<Generation>>>,
) -> Result<Arc<Generation>> {
    match outcome {
        Outcome::Done(result) => result,
        Outcome::Dropped => Err(anyhow::anyhow!(
            "warm writer queue is shutting down; generation open abandoned"
        )),
        Outcome::Panicked => Err(anyhow::anyhow!(
            "warm writer queue panicked while opening a generation"
        )),
    }
}

/// Open a fresh [`Generation`]: the held-open cache connection plus the `(dev,
/// ino)` identity and index identity we will verify it against on later
/// requests. Called only from the writer-queue generation-open op
/// ([`open_or_adopt`]), serialized on the per-vault writer thread.
///
/// # Sentinel discipline (load-bearing)
///
/// The identity we store must reflect the inode the *live connection* is attached
/// to; getting this wrong yields either spurious reopens or, worse, an undetected
/// ghost (a connection serving a unlinked-but-alive database while the path holds
/// a different file). Two cases, handled differently for exactly this reason:
///
/// - **Existing cache (the steady-state daemon case).** Open the sentinel
///   `File` on `cache.db` BEFORE constructing the `Cache`, and store the
///   sentinel's inode. Rationale: sentinel-before-connection means that if a
///   `cache clear` races the open (swapping the inode), the sentinel captured the
///   old inode while the connection attaches to the new one — the next request's
///   ground-shift sees path != stored and heals with at worst ONE spurious
///   reopen. The reverse order (connection-before-sentinel) could store the NEW
///   inode while the connection sits on the OLD (unlinked) one, and the path
///   would match forever — an UNDETECTED ghost. In the common no-race case the
///   `Cache` reuses the same inode, so stored == connection == path and there are
///   zero spurious reopens.
///
/// - **Absent cache (first-ever touch).** There is no file to pre-open, and
///   pre-touching an empty file would force `Cache::open` down its
///   corrupt/rebuild path (empty file has no schema), swapping the inode and
///   leaving the sentinel one inode behind — which would cost a spurious reopen
///   on the very next request and break verify-once for brand-new vaults.
///   Instead, let `Cache::open` create the database cleanly (`open_fresh`, single
///   inode, no rebuild churn), THEN open the sentinel and store the live inode.
///   The only residual race — an external `cache clear` in the microsecond window
///   between create and capture, on a just-created cache — self-heals via a later
///   ground-shift and is accepted as negligible (a brand-new cache is not a clear
///   target in practice).
pub(in crate::env) fn open_generation(
    vault_root: &Utf8Path,
    config: &LoadedConfig,
    number: u64,
) -> Result<Generation> {
    let (_canonical, cache_dir) = cache_dir_for(vault_root)?;
    ensure_cache_dir(&cache_dir)?;
    let db_path = cache_dir.join("cache.db");

    let opts = &config.index_options;
    let existed = db_path.as_std_path().exists();

    let (cache, sentinel) = if existed {
        // Existing cache: sentinel BEFORE connection (ghost-safe ordering).
        let sentinel = File::open(db_path.as_std_path())?;
        let cache = Cache::open_with_index(
            vault_root,
            opts.alias_field.as_deref(),
            &opts.ignore,
            &opts.resolved_index_set,
            &opts.resolved_index_set_hash,
        )?;
        (cache, sentinel)
    } else {
        // First touch: let Cache::open create it cleanly, then capture the live
        // inode via a post-open sentinel (see doc comment for why).
        let cache = Cache::open_with_index(
            vault_root,
            opts.alias_field.as_deref(),
            &opts.ignore,
            &opts.resolved_index_set,
            &opts.resolved_index_set_hash,
        )?;
        let sentinel = File::open(db_path.as_std_path())?;
        (cache, sentinel)
    };

    let db_identity = device_inode(&sentinel.metadata()?);

    // The inode the primary READ connection actually ended on — the LIVE path
    // inode right after the primary open. This is NOT necessarily `db_identity`:
    // the sentinel is opened BEFORE the primary open (ghost-safe ordering), so a
    // rebuild-on-open (alias / schema / identity drift → delete+recreate) leaves
    // `db_identity` on the pre-rebuild inode while the read connection — and the
    // live path — moved to the post-rebuild inode. It is this live inode the
    // companion must match to share one generation with the read connection.
    let read_identity = device_inode(&std::fs::metadata(db_path.as_std_path())?);

    // Open the WRITE connection: a second connection to the SAME cache.db the
    // primary open just verified under the now-held sentinel, skipping the
    // O(db-size) integrity_check (see `Cache::open_companion_verified`), and
    // RECONCILED against the live post-primary inode (`open_companion_on_inode`).
    // Only the writer thread's freshness refresh writes through it; the pooled read
    // connections stay read-only (NRN-252). This is NOT made `query_only` — it is
    // the one connection that writes.
    let write_cache = open_companion_on_inode(
        vault_root,
        opts.alias_field.as_deref(),
        &opts.ignore,
        &opts.resolved_index_set,
        &opts.resolved_index_set_hash,
        read_identity,
    )?;

    // Seed the read pool with the primary READ connection this call already opened
    // and verified (do NOT open it twice). `ReadPool::seed` stamps `query_only` on
    // it as it enters the pool — now, after all open-time verification / rebuild /
    // reshred are complete. The pool lazily grows additional read connections via
    // the SAME `open_companion_on_inode` reconciliation guard, comparing against the
    // live `read_identity` the primary landed on.
    // Derive the index identity ONCE and share it between the generation and its
    // pool's grow params, so every lazily-grown connection provably opens under
    // the exact identity the generation records.
    let index_identity = IndexIdentity::from_config(config);
    let read_pool = ReadPool::seed(
        cache,
        GrowParams {
            vault_root: vault_root.to_owned(),
            identity: index_identity.clone(),
            index_set: opts.resolved_index_set.clone(),
            expected_identity: read_identity,
        },
        read_pool_cap(),
    )?;

    Ok(Generation {
        number,
        db_identity,
        index_identity,
        _sentinel: sentinel,
        read_pool,
        write_cache: Mutex::new(write_cache),
        refresh_pending: Mutex::new(None),
        #[cfg(test)]
        refresh_exec_count: AtomicU64::new(0),
        #[cfg(test)]
        refresh_arrivals: AtomicU64::new(0),
        #[cfg(test)]
        inject_refresh_error: Mutex::new(None),
        #[cfg(test)]
        inject_increment_error: Mutex::new(None),
        #[cfg(test)]
        inject_increment_panic: AtomicBool::new(false),
        #[cfg(test)]
        refresh_gate: Mutex::new(None),
        #[cfg(test)]
        last_refresh_report: Mutex::new(None),
        #[cfg(test)]
        increment_gate: Mutex::new(None),
    })
}

/// Create the cache directory (0700 on unix) if absent, so the sentinel and
/// `cache.db` can be opened. Mirrors the security posture `Cache::open` applies.
pub(in crate::env) fn ensure_cache_dir(cache_dir: &Utf8Path) -> Result<()> {
    std::fs::create_dir_all(cache_dir.as_std_path())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            cache_dir.as_std_path(),
            std::fs::Permissions::from_mode(0o700),
        )?;
    }
    Ok(())
}

impl VaultContext {
    /// The ONE single-flight generation-open path (steps 2–3). Returns the
    /// current generation, opening N+1 when the current one is absent, ground-
    /// shifted, invalidated (below the floor), or opened under a now-different
    /// index identity.
    ///
    /// The lock-free fresh fast path returns without touching the queue. On stale,
    /// it submits a generation-open **liveness op** and blocks on it (callers run
    /// inside `spawn_blocking` tool bodies, so a blocking wait is correct). The
    /// writer thread runs opens one at a time, so concurrent stale observers
    /// coalesce to exactly one open: the op re-checks the current pointer first, so
    /// a late arrival adopts the generation an earlier op just produced (NRN-252).
    /// A dropped op (queue shutting down) or a panicked op surfaces as a
    /// descriptive error rather than a hang — see [`map_open_outcome`].
    pub(in crate::env) fn ensure_current(
        &self,
        slot: &WarmSlot,
        scope: &RequestScope,
    ) -> Result<Arc<Generation>> {
        // The request's bound config (NRN-253) — the whole reopen decision uses
        // ONE config `Arc` for the request, so a concurrent config swap cannot
        // race this generation-open.
        let config = scope.config();

        // Fast path: probe the current generation off any lock.
        {
            let snapshot = slot
                .shared
                .current
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            if let Some(generation) = &snapshot {
                if generation_is_fresh(generation, &slot.shared.floor, &self.vault_root, &config) {
                    return Ok(Arc::clone(generation));
                }
            }
        }

        // Stale or cold → queue a generation-open op and block on its outcome.
        map_open_outcome(self.spawn_generation_open(slot, config).wait())
    }

    /// Submit the generation-open liveness op `ensure_current` blocks on. Split out
    /// so the op's captured state is assembled in one place (and so a test can
    /// model a caller blocked on the handle across a queue shutdown). The op runs
    /// `'static` on the writer thread, so it captures owned / `Arc` state — never
    /// `&self` or `&slot`.
    fn spawn_generation_open(
        &self,
        slot: &WarmSlot,
        config: Arc<LoadedConfig>,
    ) -> Handle<Result<Arc<Generation>>> {
        let shared = Arc::clone(&slot.shared);
        let vault_root = self.vault_root.clone();
        slot.queue
            .submit_liveness(move || open_or_adopt(&shared, &vault_root, &config))
    }

    /// Test-only accessor for the identity of the current warm generation's cache
    /// (`None` in cold mode or before the first open).
    #[cfg(test)]
    pub(crate) fn warm_db_identity(&self) -> Option<(u64, u64)> {
        self.current_generation().map(|g| g.db_identity)
    }

    /// Test-only accessor: a clone of the current generation `Arc` (`None` in cold
    /// mode or before the first open). Lets a test hold a generation to model an
    /// in-flight request and prove drain-and-drop.
    #[cfg(test)]
    pub(crate) fn current_generation(&self) -> Option<Arc<Generation>> {
        match &self.mode {
            Mode::Warm(slot) => slot
                .shared
                .current
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone(),
            Mode::Cold => None,
        }
    }

    /// Test-only accessor: total generation opens (cold start + every reopen).
    /// Drives the single-flight / coalescing assertions.
    #[cfg(test)]
    pub(crate) fn generation_opens(&self) -> u64 {
        match &self.mode {
            Mode::Warm(slot) => slot.shared.open_count.load(Ordering::Relaxed),
            Mode::Cold => 0,
        }
    }

    /// Test-only: read connections the CURRENT generation's pool has lazily grown
    /// BEYOND its seed (`0` if none / cold). A value `> 0` proves concurrent warm
    /// reads genuinely overlapped on distinct pooled connections rather than
    /// accidentally serializing (NRN-253).
    #[cfg(test)]
    pub(crate) fn current_read_pool_grow_opens(&self) -> u64 {
        self.current_generation()
            .map(|g| g.read_pool.grow_opens.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Test-only: the warm slot's writer queue (panics off warm mode). Lets a test
    /// occupy the writer thread with a blocker so a later generation-open or refresh
    /// op stays queued — used by both this module's and the `server` module's
    /// concurrency proofs.
    #[cfg(test)]
    pub(crate) fn warm_writer_queue(&self) -> &WriterQueue {
        match &self.mode {
            Mode::Warm(slot) => &slot.queue,
            Mode::Cold => panic!("warm_writer_queue called on a cold context"),
        }
    }

    /// Test-only: submit a generation-open op exactly as `ensure_current` does,
    /// returning the raw queue handle (not the mapped `Result`), so a test can
    /// model a caller blocked on the op while the queue shuts down under it.
    #[cfg(test)]
    pub(in crate::env) fn submit_generation_open(&self) -> Handle<Result<Arc<Generation>>> {
        match &self.mode {
            Mode::Warm(slot) => self.spawn_generation_open(slot, self.config()),
            Mode::Cold => panic!("submit_generation_open called on a cold context"),
        }
    }

    /// Test-only: run `ensure_current` WITHOUT binding `bound_generation`,
    /// unlike `query_cache`. Models a request (or, under NRN-253, a concurrent
    /// reader) that advances `slot.current` to a newer generation while a
    /// DIFFERENT, already-in-flight request's `bound_generation` still points at
    /// an older one — the scenario `note_tool_error` must not misattribute.
    #[cfg(test)]
    pub(crate) fn force_reopen_without_binding(&self) -> Result<()> {
        match &self.mode {
            // A throwaway scope carries only the bound config `ensure_current`
            // needs; it is NOT the caller's scope, so no `bound_generation` is
            // stamped anywhere — modeling a reopen that advances `current`
            // without binding it to the errored request.
            Mode::Warm(slot) => {
                let scope = RequestScope::new(self.config());
                self.ensure_current(slot, &scope).map(|_| ())
            }
            Mode::Cold => Ok(()),
        }
    }

    /// Test-only: bump the invalidation floor above the current generation, making
    /// it stale — the [`generation_still_current_guard`] then reads false. Models
    /// an out-of-band eviction (corruption / `cache clear`) mid-commit.
    #[cfg(test)]
    pub(crate) fn test_invalidate_current_generation(&self) {
        if let Mode::Warm(slot) = &self.mode {
            if let Some(g) = slot
                .shared
                .current
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
            {
                slot.shared.floor.fetch_max(g.number + 1, Ordering::AcqRel);
            }
        }
    }
}
