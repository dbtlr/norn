//! Warm-cache generations, connection pools, and per-vault slots.
//!
//! ### Generational contexts (ADR 0013, NRN-251)
//!
//! The held cache state is an **immutable [`Generation`]**: once opened, the
//! `(cache identity, index set it was opened under, sentinel)` it carries never
//! mutates. (The DB *content* still changes — the writer-queue freshness refresh
//! writes through the generation's dedicated WRITE connection — but the *binding*
//! is fixed.) Every request binds the current `Arc<Generation>` (and the current
//! config `Arc`) at its boundary and holds both to completion, so no request ever
//! observes a swap mid-flight.
//!
//! Each generation holds a POOL of request-facing READ connections plus one
//! writer-thread-only WRITE connection (ADR 0013: "N read connections + 1 write
//! connection per generation"). After NRN-252 the request path never writes
//! through a read connection — the freshness refresh moved onto the write
//! connection — so the read side is **read-only in practice**; NRN-253 both pools
//! it (so concurrent reads no longer serialize on a single connection's mutex once
//! `call_lock` retires) and ENFORCES the read-only invariant with
//! `PRAGMA query_only = ON` on every pooled connection.
//!
//! Every evict/re-open trigger — cold start / first touch, ground-shift
//! (out-of-band `cache clear` / `prune` / `rm`), cache-identity change /
//! corruption, and an index-relevant config change — routes through the ONE
//! single-flight path [`VaultEnv::ensure_current`], which opens generation
//! N+1 and swaps the slot's current pointer. The open itself is a **writer-queue
//! liveness op** (ADR 0013 Phase 2, NRN-252): a stale `ensure_current` submits a
//! generation-open op and blocks on it, and the per-vault writer thread runs opens
//! one at a time. Serialization by that thread — not a mutex guard — is what
//! coalesces concurrent opens now: N stale observers of generation N submit N ops;
//! the first opens N+1 and swaps, the rest re-check the current pointer, find it
//! fresh, and adopt it. In-flight requests keep serving on N; N drops with
//! its last `Arc`, closing its connection and sentinel fd via `Drop`. There are
//! no in-place "null the slot" eviction sites: a trigger either changes what
//! `ensure_current` observes (identity / config mismatch) or bumps the slot's
//! monotonic invalidation floor (corruption; a panic unwinding through a cache
//! guard) so the next request reopens through the same path.
//!
//! A **non-index config change** swaps the stored config `Arc` for future
//! requests without opening a new generation — the generation's index identity
//! still matches, so `ensure_current` reuses it; in-flight requests keep the
//! config they bound.
//!
//! **Named non-goal (ADR 0013):** any future streaming / subscription surface
//! must be generation-aware from birth — its source of truth can be swapped under
//! it mid-stream. Nothing here accommodates streams.
//!

use super::*;

/// The index-relevant inputs to `Cache::open_with_index` that determine cache
/// CONTENT. A generation records the identity it was opened under; a config
/// change whose new identity differs (resolved index-set hash, `alias_field`, or
/// `files.ignore`) is index-relevant and forces a new generation, while any
/// other config change is just a config `Arc` swap (the generation is reused).
#[derive(Clone, PartialEq, Eq, Debug)]
pub(in crate::env) struct IndexIdentity {
    /// The resolved index-set hash — a function of the whole resolved set, so
    /// comparing it covers the entire field set.
    pub(in crate::env) index_set_hash: String,
    pub(in crate::env) alias_field: Option<String>,
    pub(in crate::env) ignore: Vec<String>,
}

impl IndexIdentity {
    pub(in crate::env) fn from_config(config: &LoadedConfig) -> Self {
        let opts = &config.index_options;
        Self {
            index_set_hash: opts.resolved_index_set_hash.clone(),
            alias_field: opts.alias_field.clone(),
            ignore: opts.ignore.clone(),
        }
    }

    /// Field-by-field equality against a live config's resolved index options —
    /// exactly the fields `from_config` would clone into a new `IndexIdentity`,
    /// compared in place. Used on the per-request freshness hot path
    /// (`generation_is_fresh`, run on every warm read), which only needs a `==`
    /// and has no use for an owned copy.
    pub(in crate::env) fn matches_config(&self, config: &LoadedConfig) -> bool {
        let opts = &config.index_options;
        self.index_set_hash == opts.resolved_index_set_hash
            && self.alias_field == opts.alias_field
            && self.ignore == opts.ignore
    }
}

/// An immutable warm-cache generation (ADR 0013). Once opened, the
/// `(cache identity, index identity, sentinel)` binding never mutates; a request
/// binds the current `Arc<Generation>` at its boundary and holds it to
/// completion. The DB *content* still changes — the writer-queue freshness
/// refresh writes through the generation's WRITE connection — which is why the
/// read pool and write connection sit behind their own synchronization;
/// everything else is fixed for the generation's life. Dropping the last `Arc`
/// closes every connection (the whole read pool + the write connection) and the
/// sentinel fd via `Drop`, so an evicted generation releases exactly when its last
/// in-flight request finishes draining on it.
///
/// # N read connections + 1 write connection (ADR 0013, NRN-253)
///
/// A generation holds a read POOL plus one write connection to the same
/// `cache.db`:
///
/// - [`read_pool`](Self::read_pool) — the request-facing READ connections. A
///   request checks one out for its whole (sync) body and returns it on drop; the
///   pool lazily grows to a small cap under concurrent load. After NRN-252 the
///   request path never writes through a read connection (the freshness refresh
///   moved off it), and NRN-253 additionally stamps `PRAGMA query_only = ON` on
///   every pooled connection, so read-only is ENFORCED. See [`ReadPool`].
/// - [`write_cache`](Self::write_cache) — the WRITE connection, touched ONLY by
///   the writer thread's ops: the freshness-refresh op and the post-apply
///   cache-increment commit (NRN-252 / NRN-158). WAL mode makes the pooled read
///   connections observe its committed rows across the connection boundary, so a
///   refresh (or increment) awaited before a read hands back fresh data.
pub(crate) struct Generation {
    /// Monotonic generation number (1 for the first open, incremented per
    /// reopen). Used to coalesce concurrent opens and to gate corruption/panic
    /// invalidation against the slot's floor.
    pub(in crate::env) number: u64,
    /// `(dev, ino)` of `cache.db` at open; compared per-request for ground-shift.
    pub(in crate::env) db_identity: (u64, u64),
    /// The index identity this generation was opened under; compared against the
    /// live config per-request to detect an index-relevant change.
    pub(in crate::env) index_identity: IndexIdentity,
    /// Held-open handle to `cache.db` captured at open. Holding it keeps the
    /// inode meaningful for the generation's life (its fstat produced
    /// `db_identity`); never read again, so `_sentinel` documents intent and
    /// suppresses dead-field lints.
    pub(in crate::env) _sentinel: File,
    /// The request-facing READ connection POOL (NRN-253). A request checks out an
    /// owned [`PooledConn`] for its whole (sync) tool body and returns it on drop;
    /// the checkout keeps this generation's `Arc` alive (via the [`WarmGuard`]) so a
    /// concurrent generation swap of the slot's current pointer cannot close the
    /// connection out from under an in-flight request. Every pooled connection is
    /// `query_only`, so read-only is enforced; a panic while a tool holds a checkout
    /// is handled by [`WarmGuard::drop`] (invalidation floor bump), independent of
    /// the pool. Behind an `Arc` so an outstanding checkout can return itself even
    /// while the generation is mid-drop.
    pub(in crate::env) read_pool: Arc<ReadPool>,
    /// The WRITE connection, a second connection to the same `cache.db` opened
    /// via the verification-skipping companion path (see
    /// `Cache::open_companion_verified`). Written through ONLY by the writer-queue
    /// ops — the freshness-refresh op ([`run_refresh_op`]) and the post-apply
    /// increment commit ([`run_increment_op`], NRN-252 / NRN-158) — which run
    /// serialized on the per-vault writer thread, so a plain `std::sync::Mutex` is
    /// always uncontended here; it exists only to satisfy `&mut` through the
    /// shared `Arc<Generation>`.
    pub(in crate::env) write_cache: Mutex<Cache>,
    /// Coalescing state for freshness refreshes on this generation (NRN-252).
    /// Holds the single in-flight-or-queued [`RefreshTicket`] that arriving
    /// requesters may join, or `None` when no refresh is pending (or the pending
    /// one has already started its scan). Guarded so the join decision is atomic
    /// against an op's start transition — see [`arrive_refresh`](VaultEnv::arrive_refresh).
    pub(in crate::env) refresh_pending: Mutex<Option<Arc<RefreshTicket>>>,
    /// Test-only: total freshness-refresh executions that actually reached
    /// `index_incremental` on this generation. Drives the coalescing /
    /// arrival-correctness assertions (exactly-one-execution counts).
    #[cfg(test)]
    pub(in crate::env) refresh_exec_count: AtomicU64,
    /// Test-only: total requesters that arrived at this generation's coalesced
    /// refresh (submitted OR joined), bumped at the top of `arrive_refresh`. Lets a
    /// full-pipeline concurrency test wait deterministically until BOTH stale
    /// readers have arrived before releasing a writer-queue blocker, so their
    /// coalescing onto one refresh is observable end-to-end (NRN-253).
    #[cfg(test)]
    pub(in crate::env) refresh_arrivals: AtomicU64,
    /// Test-only: a one-shot error the next refresh op returns INSTEAD of running
    /// `index_incremental`, so a test can drive a corruption-class refresh failure
    /// through the ticket and prove `note_tool_error` still classifies it.
    #[cfg(test)]
    pub(in crate::env) inject_refresh_error: Mutex<Option<CacheError>>,
    /// Test-only: a one-shot error the next increment-commit chunk returns INSTEAD
    /// of committing, so a test can drive a corruption-class increment failure
    /// deterministically and prove the eviction targets the generation the
    /// increment ran on (NRN-253 review).
    #[cfg(test)]
    pub(in crate::env) inject_increment_error: Mutex<Option<CacheError>>,
    #[cfg(test)]
    pub(in crate::env) inject_increment_panic: AtomicBool,
    /// Test-only: a one-shot gate the next refresh op waits on AFTER its start
    /// transition (started flag set, pending cleared) and BEFORE its scan, so a
    /// test can hold a refresh "in flight" while a new requester arrives.
    #[cfg(test)]
    pub(in crate::env) refresh_gate: Mutex<Option<TestGate>>,
    /// Test-only: the `IndexReport` the most recent freshness refresh produced,
    /// captured so the NRN-158 acceptance test can assert an empty report (zero
    /// changes ⇒ no whole-vault rebuild) after a warm mutation committed its
    /// increment.
    #[cfg(test)]
    pub(in crate::env) last_refresh_report: Mutex<Option<crate::cache::IndexReport>>,
    /// Test-only: a reusable gate the increment-commit op signals + waits on at
    /// EACH chunk boundary (after TEMP staging, before the next entry), so a test
    /// can observe main staying old, interleave a liveness op, or turn the
    /// generation stale mid-stage — all without sleeps.
    #[cfg(test)]
    pub(in crate::env) increment_gate: Mutex<Option<TestGate>>,
}

impl Generation {
    /// Check out a read-only connection from this generation's pool for the life of
    /// one request. Infallible: a saturated pool (or a failed lazy grow) makes a
    /// reader WAIT for a checkin, never fail — the pre-NRN-253 exclusive-guard
    /// semantics. See [`ReadPool`].
    pub(in crate::env) fn checkout_read(&self) -> PooledConn {
        self.read_pool.checkout()
    }

    /// Test-only: check out a read connection from this generation's pool. Lets a
    /// test inspect a pooled connection directly the way it used to `lock()` the
    /// single mutex.
    #[cfg(test)]
    pub(in crate::env) fn test_read_conn(&self) -> PooledConn {
        self.read_pool.checkout()
    }
}

/// Hard ceiling on the number of READ connections a single generation's
/// [`ReadPool`] may open, before clamping to the host's available parallelism —
/// see [`read_pool_cap`]. A small fixed cap: enough concurrency for realistic
/// read fan-out without unbounded connection growth against one `cache.db`.
const READ_POOL_MAX: usize = 8;

/// Debug/test-only override for [`read_pool_cap`] (read via
/// [`debug_env_usize`](crate::cache::debug_env_usize), so release builds ignore it
/// entirely). Lets a test force a tiny cap to prove wait-at-cap behavior
/// deterministically.
const READ_POOL_CAP_ENV: &str = "NORN_READ_POOL_CAP";

/// The per-generation read-connection cap: `min(READ_POOL_MAX, available
/// parallelism)`, floored at 1. Now that warm mode has retired `call_lock`
/// (NRN-253), concurrent warm reads run in parallel and the pool lazily grows up
/// to this cap under real read fan-out. Debug builds honor the
/// `NORN_READ_POOL_CAP` override so a test can pin the cap.
pub(in crate::env) fn read_pool_cap() -> usize {
    let parallelism = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let default = parallelism.min(READ_POOL_MAX);
    crate::cache::debug_env_usize(READ_POOL_CAP_ENV, default).max(1)
}

/// Test-only: pin the process-global `NORN_READ_POOL_CAP` override for the guard's
/// life, serialized on a shared lock so cap-sensitive tests in this module AND the
/// `server` concurrency suite never clobber each other's cap mid-open (the env var
/// is process-global). The override is removed on drop. Held across `.await` is
/// fine — a `#[tokio::test]` future runs via `block_on`, which imposes no `Send`
/// bound, so a non-`Send` guard in the test frame does not constrain it.
#[cfg(test)]
pub(crate) struct ReadPoolCapGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl ReadPoolCapGuard {
    pub(crate) fn pin(cap: usize) -> Self {
        static LOCK: Mutex<()> = Mutex::new(());
        let lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(READ_POOL_CAP_ENV, cap.to_string());
        Self { _lock: lock }
    }
}

#[cfg(test)]
impl Drop for ReadPoolCapGuard {
    fn drop(&mut self) {
        std::env::remove_var(READ_POOL_CAP_ENV);
    }
}

/// A per-generation POOL of read-only connections to `cache.db` (ADR 0013 /
/// NRN-253). Replaces the pre-NRN-253 single `Arc<Mutex<Cache>>` handed out as one
/// exclusive owned guard per request — which, once `call_lock` retired (NRN-253),
/// would have serialized every warm read on that mutex — with a small set of
/// interchangeable `query_only` connections to the same database.
///
/// - **Checkout** ([`checkout`](Self::checkout)) pops an idle connection (LIFO —
///   warmest first) and hands back an owned [`PooledConn`]; the connection leaves
///   the pool for the request's life and returns on drop, so there is no
///   per-request connection churn.
/// - **Lazy grow.** When no connection is idle and the pool is under [`cap`], one
///   more connection is opened via [`open_companion_on_inode`] — the same inode
///   reconciliation guard the write connection uses — and stamped `query_only`.
///   A grow FAILURE never fails the checkout: it degrades to waiting for a
///   checkin (the pool is seeded, so a holder always exists), because handing
///   out a read guard was infallible pre-pool and an inode-swap race mid-`cache
///   clear` must keep succeeding the way the single seed connection does.
/// - **Wait at cap.** At cap with none idle, checkout blocks on
///   [`available`](Self::available) until a checkin, then retries. The wait is
///   unfair/barging — a newly-arriving checkout can pop an idle connection ahead
///   of a woken waiter — exactly like the `parking_lot` mutex it replaced (which
///   is also barging, not FIFO).
/// - **Read-only by construction.** Every pooled connection is `query_only`, so a
///   write through it errors; the writer thread's WRITE connection lives on
///   [`Generation`] outside the pool and is the only connection that writes.
///
/// The pool (and its connections) drop with the generation; an outstanding
/// [`PooledConn`] holds an `Arc<ReadPool>` so it can still check itself back in
/// even while the generation is mid-drop.
pub(in crate::env) struct ReadPool {
    /// Idle connections + the total-connection count, together under one lock so
    /// the "grow vs wait" decision is atomic.
    inner: Mutex<ReadPoolInner>,
    /// Signalled on every checkin, waking a checkout blocked at cap.
    available: Condvar,
    /// Maximum live connections (idle + checked-out). See [`read_pool_cap`].
    cap: usize,
    /// How to open (and inode-verify) an additional read connection on grow.
    grow: GrowParams,
    /// Test-only: connections opened via lazy-grow (NOT counting the seed).
    /// Proves checkout reuse (stays 0 across sequential checkout/drop/checkout) and
    /// growth (increments when a second concurrent checkout materializes one).
    #[cfg(test)]
    pub(in crate::env) grow_opens: AtomicU64,
}

/// The mutable interior of a [`ReadPool`], guarded as a unit.
struct ReadPoolInner {
    /// Connections available for immediate checkout (LIFO).
    idle: Vec<Cache>,
    /// Total connections owned by the pool (idle + checked-out). Gates growth
    /// against [`ReadPool::cap`]; a reserved (incremented) slot during an in-flight
    /// grow prevents the pool overshooting the cap.
    total: usize,
}

/// The inputs to open one additional read connection on lazy grow: the index
/// identity the seed connection was opened under (embedded as the generation's
/// own [`IndexIdentity`], so a future identity field cannot be added there and
/// missed here — a lazily-grown connection opening under a stale identity is
/// exactly the drift this guards), the resolved index set backing that
/// identity's hash, plus the live `cache.db` inode the seed landed on. A grown
/// companion MUST land on that same inode (an out-of-band `cache clear`
/// swapping the file mid-request is caught here), mirroring the write
/// connection's reconciliation. Owned (cloned from config at open) so the pool
/// is self-contained and needs no `&LoadedConfig` at grow time.
pub(in crate::env) struct GrowParams {
    pub(in crate::env) vault_root: Utf8PathBuf,
    /// The index identity every grown connection must open under — the SAME
    /// value stored on the owning [`Generation`].
    pub(in crate::env) identity: IndexIdentity,
    /// The resolved index set whose hash is `identity.index_set_hash`; the
    /// companion open needs the concrete set, not just its hash.
    pub(in crate::env) index_set: BTreeSet<String>,
    pub(in crate::env) expected_identity: (u64, u64),
}

impl GrowParams {
    /// Open one additional read connection and stamp it `query_only`.
    fn open(&self) -> Result<Cache> {
        let cache = open_companion_on_inode(
            &self.vault_root,
            self.identity.alias_field.as_deref(),
            &self.identity.ignore,
            &self.index_set,
            &self.identity.index_set_hash,
            self.expected_identity,
        )?;
        cache.set_query_only()?;
        Ok(cache)
    }
}

impl ReadPool {
    /// Seed the pool with the generation's already-opened-and-verified primary read
    /// connection (do NOT open it twice). `query_only` is applied to the primary
    /// HERE — as it enters the pool, after all open-time verification / rebuild /
    /// reshred are complete.
    pub(in crate::env) fn seed(primary: Cache, grow: GrowParams, cap: usize) -> Result<Arc<Self>> {
        primary.set_query_only()?;
        Ok(Arc::new(ReadPool {
            inner: Mutex::new(ReadPoolInner {
                idle: vec![primary],
                total: 1,
            }),
            available: Condvar::new(),
            cap,
            grow,
            #[cfg(test)]
            grow_opens: AtomicU64::new(0),
        }))
    }

    /// Check out a connection: reuse an idle one, else lazily grow while under cap,
    /// else block until a checkin frees one. Grows OUTSIDE the lock (reserving the
    /// slot first) so a slow open never blocks a concurrent checkin. Infallible: a
    /// grow FAILURE releases its reserved slot and degrades this checkout to
    /// waiting for a checkin instead of failing the read — always live, because
    /// the pool is seeded (total >= 1) so some holder will check its connection
    /// back in. The wait is unfair/barging (see the type docs).
    pub(in crate::env) fn checkout(self: &Arc<Self>) -> PooledConn {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        // Cleared after a failed grow: this checkout then only waits for a
        // checkin rather than re-attempting an open that just failed (a
        // concurrent `cache clear` inode swap resolves via the next generation,
        // not by retrying here).
        let mut try_grow = true;
        loop {
            if let Some(cache) = inner.idle.pop() {
                return PooledConn {
                    cache: Some(cache),
                    pool: Arc::clone(self),
                };
            }
            if try_grow && inner.total < self.cap {
                // Reserve the slot, then open with the lock released.
                inner.total += 1;
                drop(inner);
                match self.grow.open() {
                    Ok(cache) => {
                        #[cfg(test)]
                        self.grow_opens.fetch_add(1, Ordering::Relaxed);
                        return PooledConn {
                            cache: Some(cache),
                            pool: Arc::clone(self),
                        };
                    }
                    Err(error) => {
                        // Degrade, never fail: surface the failure on the
                        // daemon's stderr, release the reserved slot, wake a
                        // waiter that may now grow into it, and fall through to
                        // wait for a checkin ourselves.
                        eprintln!(
                            "norn serve: read-pool grow failed ({error:#}); \
                             waiting for an idle connection instead"
                        );
                        try_grow = false;
                        inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
                        inner.total -= 1;
                        self.available.notify_one();
                        continue;
                    }
                }
            }
            // At cap (or grow degraded), none idle → wait for a checkin, then
            // re-evaluate.
            inner = self
                .available
                .wait(inner)
                .unwrap_or_else(|p| p.into_inner());
        }
    }

    /// Return a connection to the idle set and wake one waiter. Called by
    /// [`PooledConn::drop`].
    fn checkin(&self, cache: Cache) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.idle.push(cache);
        drop(inner);
        self.available.notify_one();
    }
}

/// An owned, checked-out read-only connection from a generation's [`ReadPool`]
/// (NRN-253). Derefs to the underlying [`Cache`]; on drop it returns the
/// connection to its pool (no per-request connection churn). Holds an
/// `Arc<ReadPool>` so an outstanding checkout can always check itself back in, even
/// if the generation is dropping concurrently.
pub(crate) struct PooledConn {
    /// The checked-out connection. `Option` so `Drop` can move it back into the
    /// pool; always `Some` until then.
    cache: Option<Cache>,
    pool: Arc<ReadPool>,
}

impl Deref for PooledConn {
    type Target = Cache;
    fn deref(&self) -> &Cache {
        self.cache
            .as_ref()
            .expect("pooled connection present until drop")
    }
}

impl DerefMut for PooledConn {
    fn deref_mut(&mut self) -> &mut Cache {
        self.cache
            .as_mut()
            .expect("pooled connection present until drop")
    }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if let Some(cache) = self.cache.take() {
            self.pool.checkin(cache);
        }
    }
}

/// Open an additional connection to `<cache_dir>/cache.db` via the
/// verification-skipping companion path ([`Cache::open_companion_verified`]) and
/// RECONCILE its inode against `expected`. The primary open already verified the
/// bytes under the held sentinel, but an fd pins the inode, NOT the PATH — so an
/// external unlink+recreate (`cache clear`) between opens could bind this by-path
/// open to a DIFFERENT inode. On mismatch the open fails so the caller retries
/// against one consistent inode; a swap AFTER this check is caught by the next
/// request's ground-shift check. Shared by the generation's WRITE connection and
/// every lazily-grown READ connection (NRN-252 / NRN-253), so the reconciliation
/// lives in exactly one place.
pub(in crate::env) fn open_companion_on_inode(
    vault_root: &Utf8Path,
    alias_field: Option<&str>,
    files_ignore: &[String],
    index_set: &BTreeSet<String>,
    index_set_hash: &str,
    expected: (u64, u64),
) -> Result<Cache> {
    let (_canonical, cache_dir) = cache_dir_for(vault_root)?;
    let db_path = cache_dir.join("cache.db");
    let cache = Cache::open_companion_verified(
        vault_root,
        alias_field,
        files_ignore,
        index_set,
        index_set_hash,
    )?;
    let identity = device_inode(&std::fs::metadata(db_path.as_std_path())?);
    if identity != expected {
        anyhow::bail!(
            "cache.db at {db_path} was swapped between companion opens \
             (identity {expected:?} → {identity:?}); failing so the caller retries \
             on a consistent inode"
        );
    }
    Ok(cache)
}

/// The slice of warm-slot state a writer-queue generation-open op touches. It
/// lives behind an `Arc` shared by the [`WarmSlot`] and every submitted open op,
/// because the op runs `'static` on the writer thread and cannot borrow the slot
/// (NRN-252). Serialization by the queue — not a mutex guard — is what coalesces
/// concurrent opens now, so these fields only need interior mutability for the
/// reads and the single swap the serialized op performs.
pub(in crate::env) struct SharedSlot {
    /// The current generation, `None` only before the first open. Never nulled in
    /// place afterward: a reopen swaps this pointer to N+1 and the old N drops
    /// with its last in-flight `Arc`.
    pub(in crate::env) current: Mutex<Option<Arc<Generation>>>,
    /// Monotonic invalidation floor: a generation whose `number` is BELOW this is
    /// stale and must be reopened. Bumped by corruption ([`note_tool_error`]) and
    /// by a panic unwinding through a live cache guard — the two triggers with no
    /// filesystem/config signal for `ensure_current` to observe. An atomic so the
    /// per-request freshness probe reads it without taking any lock; a separate
    /// `Arc` (not just the enclosing `Arc<SharedSlot>`) so each handed-out cache
    /// guard can hold a lightweight handle whose panic-`Drop` bumps it.
    pub(in crate::env) floor: Arc<AtomicU64>,
    /// Next generation number to assign (1 for the first open). Mutated only by the
    /// serialized open op, so queue serialization alone keeps it single-flight —
    /// the former `open` guard's coalescing role is now the writer thread's.
    pub(in crate::env) next_number: Mutex<u64>,
    /// Total generation opens performed (cold start + every reopen). Test-only
    /// coalescing/single-flight assertion counter; incremented on every open.
    pub(in crate::env) open_count: AtomicU64,
}

/// Warm-mode per-vault slot. The small fields are guarded by `std::sync::Mutex`
/// (NOT tokio), locked only briefly and NEVER across an `.await` (tool bodies are
/// sync); the cache connection's own lock lives inside [`Generation`].
pub(in crate::env) struct WarmSlot {
    /// Generation-open state shared with the writer-queue open ops (see
    /// [`SharedSlot`]) — `current`, the invalidation `floor`, the generation
    /// counter, and the open count.
    pub(in crate::env) shared: Arc<SharedSlot>,
    /// Fingerprint of the config file; independent of any generation, so it
    /// survives reopens.
    pub(in crate::env) config_fp: Mutex<ConfigFingerprint>,
    /// The per-vault writer queue: the single serialization point for generation
    /// opens and freshness refreshes — subsuming the former `open` single-flight
    /// mutex — and, in a later commit, apply increments (ADR 0013, NRN-252). A
    /// stale [`ensure_current`](VaultEnv::ensure_current) submits a
    /// generation-open LIVENESS op and blocks on it; the queue runs opens one at a
    /// time, so N concurrent stale callers coalesce to one open and the rest adopt.
    /// The per-request freshness refresh is likewise a LIVENESS op on this queue,
    /// executed on the generation's write connection and coalesced per generation.
    pub(in crate::env) queue: WriterQueue,
}

/// A cache handle handed out by [`VaultEnv::query_cache`], serving both
/// modes behind one type. Derefs into the underlying [`Cache`] so callers read
/// `cache.conn()` / pass `&cache` uniformly.
// `Owned` carries a whole `Cache` (a SQLite connection) so it dwarfs the warm
// guard; a `CacheHandle` is a short-lived per-call stack value, never stored in
// bulk, so the variant-size gap does not matter here.
#[allow(clippy::large_enum_variant)]
pub(crate) enum CacheHandle {
    /// Cold mode: an owned, freshly-opened cache (dropped at end of the call).
    Owned(Cache),
    /// Warm mode: an owned guard into the bound generation's connection.
    Warm(WarmGuard),
}

impl std::fmt::Debug for CacheHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CacheHandle::Owned(_) => f.write_str("CacheHandle::Owned(..)"),
            CacheHandle::Warm(_) => f.write_str("CacheHandle::Warm(..)"),
        }
    }
}

impl Deref for CacheHandle {
    type Target = Cache;
    fn deref(&self) -> &Cache {
        match self {
            CacheHandle::Owned(cache) => cache,
            CacheHandle::Warm(guard) => guard,
        }
    }
}

impl DerefMut for CacheHandle {
    fn deref_mut(&mut self) -> &mut Cache {
        match self {
            CacheHandle::Owned(cache) => cache,
            CacheHandle::Warm(guard) => guard,
        }
    }
}

/// An owned handle onto the warm-mode bound generation's held-open cache for one
/// request.
///
/// It keeps the whole `Arc<Generation>` alive (so the sentinel fd + pooled
/// connections stay open for the request even if a concurrent open swaps the
/// slot's current pointer to a newer generation) and holds a [`PooledConn`] — one
/// read-only connection checked out of the generation's [`ReadPool`] — across the
/// entire (sync) tool body, returned to the pool when this guard drops.
///
/// `Drop` carries the generational replacement for std-mutex poison recovery: if
/// the guard is dropped while the thread is PANICKING (a tool body panicked
/// mid-work, possibly mid-mutation), it bumps the slot's invalidation floor above
/// this generation's number, so the next request reopens through
/// [`ensure_current`](VaultEnv::ensure_current) and re-verifies integrity —
/// exactly the trust-restoring self-heal the old poison-evict path gave, now
/// routed through the one open path. On a normal drop it does nothing (the pooled
/// connection is returned by [`PooledConn::drop`] regardless).
pub(crate) struct WarmGuard {
    /// Shared handle to the slot's invalidation floor, bumped on a panic-drop.
    pub(in crate::env) floor: Arc<AtomicU64>,
    /// The read-only connection checked out of the generation's pool, held across
    /// the tool body and returned to the pool on drop. It owns its own
    /// `Arc<ReadPool>`, so the connection can be checked back in independently of
    /// `generation`.
    pub(in crate::env) conn: PooledConn,
    /// Keeps the bound generation (sentinel fd + identity) alive for the whole
    /// request, so a concurrent open swapping the slot's current pointer to a
    /// newer generation cannot close this request's sentinel out from under it.
    /// Also names the generation a panic-drop invalidates (its `number`).
    pub(in crate::env) generation: Arc<Generation>,
}

impl Drop for WarmGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.floor
                .fetch_max(self.generation.number + 1, Ordering::AcqRel);
        }
    }
}

impl Deref for WarmGuard {
    type Target = Cache;
    fn deref(&self) -> &Cache {
        &self.conn
    }
}

impl DerefMut for WarmGuard {
    fn deref_mut(&mut self) -> &mut Cache {
        &mut self.conn
    }
}
