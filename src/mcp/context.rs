//! Vault context for the MCP servers (stdio `norn mcp` and the warm `norn serve`
//! daemon).
//!
//! # Two modes: cold (stdio) and warm (daemon)
//!
//! [`VaultContext`] holds a parsed [`LoadedConfig`] and hands out a cache handle
//! per tool call via [`VaultContext::query_cache`]. It has two modes, chosen at
//! construction, that differ in how much they hold open across calls.
//!
//! ## Cold mode — [`VaultContext::open`] (stdio `norn mcp`)
//!
//! Config is parsed once at startup and held for the process lifetime; a config
//! change requires a server restart, exactly like the CLI re-reads config fresh
//! on every invocation. The cache is deliberately **not** held: each tool call
//! opens a fresh [`Cache`] via `open_for_query`, paying that opener's
//! `PRAGMA integrity_check` plus the cheap incremental freshness refresh every
//! time. This matches the CLI's per-invocation behavior exactly and needs no
//! filesystem watcher. This is `norn mcp`'s v1 behavior and is unchanged.
//!
//! ## Warm mode — [`VaultContext::open_warm`] (daemon `norn serve`)
//!
//! A long-lived daemon holds one `VaultContext` open across many requests, so
//! re-paying `integrity_check` on every call is wasteful. Warm mode instead
//! **verifies trust once** (the first `integrity_check` when the cache is first
//! opened) and then **continuously maintains** it with a cheap per-request
//! self-heal pipeline, upholding the ADR-0005 trust invariant: reading through
//! norn must always feel like touching the actual files.
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
//! Each generation holds TWO connections to its `cache.db`: a request-facing
//! READ connection and a writer-thread-only WRITE connection. After NRN-252 the
//! request path never writes through the read connection — the freshness refresh
//! moved onto the write connection — so the read connection is **read-only in
//! practice**, which is the precondition NRN-253 pools it under.
//!
//! Every evict/re-open trigger — cold start / first touch, ground-shift
//! (out-of-band `cache clear` / `prune` / `rm`), cache-identity change /
//! corruption, and an index-relevant config change — routes through the ONE
//! single-flight path [`VaultContext::ensure_current`], which opens generation
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
//! ### The per-request pipeline
//!
//! The pipeline is split across two entry points so it runs ONCE per request in
//! a fixed order, no matter which tool is calling. [`VaultContext::begin_request`]
//! runs steps 0–1 at the per-request seam (the server calls it before every tool
//! body — see `mcp::server`); [`VaultContext::query_cache`] runs steps 2–4 when a
//! tool actually opens the cache. Tools that reconstruct a graph index instead of
//! running a `query_cache` filter (validate, repair, set, edit, delete, move,
//! rewrite, apply, new) go through [`VaultContext::load_graph_index`], a thin
//! composition over `query_cache` plus the cache reader — so in warm mode those
//! tools bind the SAME generation and are served verify-once too, not cold-opened
//! per request (NRN-130). Putting root-liveness + config-freshness in
//! `begin_request` means *every* tool — query-cache and graph-index alike — gets
//! them, and config stays STABLE for the whole request (no mid-request swap, so
//! one request can never mix an old-config graph index with a new-config cache).
//!
//! 0. **Root liveness** (`begin_request`)**.** Canonicalize `vault_root`; if it is
//!    gone, return a typed [`WarmContextError::RootGone`] the daemon can downcast
//!    to evict the whole context.
//! 1. **Config freshness** (`begin_request`)**.** Read `<vault_root>/.norn/config.yaml`
//!    and compare a content-hash fingerprint (blake3 of the file bytes, plus
//!    `exists`). An existing-but-unreadable config (e.g. `chmod 000`) fails
//!    *this* request too, distinctly from "absent" — see [`fingerprint_config`].
//!    Unchanged → proceed. Changed → re-parse: a parse error fails *this* request
//!    (mirroring a direct CLI invocation) and leaves the fingerprint stale so the
//!    next request retries. On a successful re-parse the stored config `Arc` is
//!    swapped unconditionally; whether the change is index-relevant (and so needs
//!    a new generation) is decided in step 3 by comparing the bound generation's
//!    index identity against the new config — begin_request drops no state.
//! 2. **Ground-shift** (`ensure_current`, via `query_cache`)**.** Stat
//!    `<cache_dir>/cache.db` and compare its `(dev, ino)` against the identity the
//!    current generation captured at open. A missing file or a mismatch makes the
//!    generation stale. This catches an out-of-band `norn cache clear` / `prune` /
//!    manual `rm` under a live daemon: POSIX keeps an unlinked file alive through
//!    the held connection, so without this check a daemon would serve a ghost
//!    database forever.
//! 3. **(Re)open if stale** (`ensure_current`)**.** If the current generation is
//!    absent, invalidated (below the floor), ground-shifted, or opened under a
//!    now-different index identity, submit a generation-open writer-queue liveness
//!    op and block on it; the serialized op opens generation N+1 (or adopts one a
//!    concurrent op just produced). See the sentinel-discipline notes on
//!    `open_generation` for the ordering that keeps identity honest. This is the
//!    ONLY place `integrity_check` is paid in warm mode.
//! 4. **Freshness** (`query_cache_warm`)**.** Split into a request-thread PROBE
//!    and, only when it fails, a REFRESH (ADR 0013 Phase 2, NRN-253). The request
//!    takes the bound generation's READ guard and runs a
//!    [`FreshnessProbe`](crate::cache::FreshnessProbe) — today the read-only
//!    stat-sweep (`crate::cache::freshness`) — against it. **Fresh** → serve on
//!    that same guard, touching neither the writer queue nor the write
//!    connection, so a concurrent read of an unchanged vault costs one stat sweep,
//!    not a refresh op (before NRN-253 every request submitted one). **Stale** →
//!    drop the read guard and run the same lock-timeout-tolerant
//!    `index_incremental` refresh cold mode gets, as a **coalesced liveness op on
//!    the per-vault writer queue** (NRN-252), executed on the generation's WRITE
//!    connection and awaited before the read guard is re-taken and handed back.
//!    Arriving requesters that share a not-yet-started refresh coalesce onto one
//!    execution via a per-generation ticket; a requester is only ever satisfied by
//!    a refresh whose scan STARTED at or after its arrival. No re-probe follows
//!    the refresh — an arrival-correct refresh IS the freshness proof, the same
//!    trust semantics the always-refresh pipeline carried. `LockTimeout` still
//!    serves anyway with the NRN-215 both-surfaces note; any other error
//!    propagates with its concrete type intact (so corruption stays classifiable
//!    by [`note_tool_error`](VaultContext::note_tool_error)). The probe is the
//!    named interface a Phase 3 watcher-events impl slots behind; the stat sweep
//!    remains the permanent demoted-mode fallback. `call_lock` still serializes
//!    tool bodies, so this concurrency is not yet observable through the MCP
//!    surface — it is real and unit-tested directly against `VaultContext`.
//!
//! ### Post-apply increment commit (NRN-252 / NRN-158)
//!
//! A warm MUTATION additionally commits its OWN cache increments after applying.
//! Each mutation tool feeds the changed-file set to
//! [`VaultContext::commit_apply_increments`], which parses the whole vault ONCE
//! **on the request thread** (no lock, off the writer thread — NRN-252 review)
//! into an `IncrementCommit`, then runs the commit as ONE **bulk** op on the
//! per-vault writer queue — a chunked closure that only commits row updates in
//! file-coherent chunks (~50ms each, WriteLock per chunk) and a final global
//! links rewrite, guarded by a `still_valid` predicate that drops the op if the
//! generation dies. Doing the parse on the request thread keeps every writer-queue
//! chunk bounded, so a liveness refresh queued behind the commit is not stalled
//! O(parse). The tool AWAITS it, so the report returns with the cache current.
//! Without this, the next read's freshness refresh would pay a full detect scan
//! AND a whole-vault rebuild (changes exist); with it, that refresh finds zero
//! changes. Failure is degraded, never propagated — the mutation already landed
//! on disk, so a deferred increment is healed by the next read (files are truth).
//!
//! Warm mode is only ever constructed with the default config location; the
//! daemon wire never carries a custom `--config` path, so `open_warm` takes only
//! `cwd` and hard-codes `config_path = None`.

use std::fs::File;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};
use parking_lot::{ArcMutexGuard, Mutex as CacheMutex};

use crate::cache::{
    cache_dir_for, Cache, CacheError, ChangeDetectOptions, Freshness, FreshnessProbe,
    StatSweepProbe,
};
use crate::cache_cmd::open_for_query;
use crate::config_loader::{load_config, LoadedConfig};
use crate::mcp::writer_queue::{ChunkOutcome, Handle, Outcome, ValidityGuard, WriterQueue};

/// A typed error the warm daemon can downcast to decide whether to evict the
/// whole `VaultContext`. Kept intentionally small and `anyhow`-downcastable.
#[derive(Debug, thiserror::Error)]
pub(crate) enum WarmContextError {
    /// The vault root can no longer be canonicalized (deleted, unmounted,
    /// permission-denied). The daemon should evict the context for this vault.
    #[error("vault root {root} is no longer accessible")]
    RootGone {
        root: Utf8PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Content-hash fingerprint of `<vault_root>/.norn/config.yaml`, used by warm
/// mode to detect config edits between requests. A fingerprint for a
/// genuinely nonexistent file carries `exists = false` and `hash = None`. An
/// EXISTING-but-unreadable file (e.g. `chmod 000`) is NOT represented here —
/// [`fingerprint_config`] returns `Err` for that case instead, so it is never
/// silently conflated with "absent".
///
/// The fingerprint is a blake3 hash of the file's bytes — NOT `(mtime, size)`.
/// A stat-based fingerprint misses a same-size rewrite within mtime granularity
/// (e.g. an editor that preserves mtime, or two writes in the same clock tick),
/// silently diverging the warm daemon from a direct CLI run. Hashing the content
/// closes that hole; a few-KB read + blake3 per `begin_request` is negligible
/// against the 50ms query budget.
#[derive(Clone, PartialEq, Eq, Debug)]
struct ConfigFingerprint {
    exists: bool,
    /// blake3 of the file bytes; `None` when the file is absent.
    hash: Option<[u8; 32]>,
}

/// Fingerprint the config file at `path` by content. A genuinely missing file
/// yields `Ok` with `exists = false`. Any OTHER read failure (permission
/// denied, I/O error, ...) is a config that EXISTS but cannot be read, which
/// must not be conflated with "absent" — that would let the daemon keep
/// silently serving stale/default config while a direct CLI invocation on the
/// same vault fails. Such failures are returned as `Err` so they propagate out
/// of `begin_request` and fail that request, mirroring direct-CLI semantics
/// (same shape as the existing parse-error path: the fingerprint is left
/// stale so the next request retries).
///
/// We deliberately read + hash rather than stat: see [`ConfigFingerprint`]
/// for why mtime/size can't gate the read.
fn fingerprint_config(path: &Utf8Path) -> Result<ConfigFingerprint> {
    match std::fs::read(path.as_std_path()) {
        Ok(bytes) => Ok(ConfigFingerprint {
            exists: true,
            hash: Some(*blake3::hash(&bytes).as_bytes()),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ConfigFingerprint {
            exists: false,
            hash: None,
        }),
        Err(error) => {
            Err(anyhow::Error::new(error).context(format!("failed to read config {path}")))
        }
    }
}

/// `(dev, ino)` of a filesystem object. On unix this uniquely identifies the
/// underlying file across unlink+recreate; a swapped inode is exactly the
/// ground-shift signal warm mode watches for.
#[cfg(unix)]
fn device_inode(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

/// Non-unix fallback: no stable `(dev, ino)`, so approximate with
/// `(len, mtime-secs)` as a best-effort change signal. norn's release targets
/// are all unix, so this path is never exercised in CI or releases; it exists
/// only to keep the crate compiling on non-unix hosts.
#[cfg(not(unix))]
fn device_inode(meta: &std::fs::Metadata) -> (u64, u64) {
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
fn current_db_identity(vault_root: &Utf8Path) -> Option<(u64, u64)> {
    let (_canonical, cache_dir) = cache_dir_for(vault_root).ok()?;
    let db_path = cache_dir.join("cache.db");
    let meta = std::fs::metadata(db_path.as_std_path()).ok()?;
    Some(device_inode(&meta))
}

/// The index-relevant inputs to `Cache::open_with_index` that determine cache
/// CONTENT. A generation records the identity it was opened under; a config
/// change whose new identity differs (resolved index-set hash, `alias_field`, or
/// `files.ignore`) is index-relevant and forces a new generation, while any
/// other config change is just a config `Arc` swap (the generation is reused).
#[derive(Clone, PartialEq, Eq, Debug)]
struct IndexIdentity {
    /// The resolved index-set hash — a function of the whole resolved set, so
    /// comparing it covers the entire field set.
    index_set_hash: String,
    alias_field: Option<String>,
    ignore: Vec<String>,
}

impl IndexIdentity {
    fn from_config(config: &LoadedConfig) -> Self {
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
    fn matches_config(&self, config: &LoadedConfig) -> bool {
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
/// caches sit behind mutexes; everything else is fixed for the generation's life.
/// Dropping the last `Arc` closes both connections and the sentinel fd via
/// `Drop`, so an evicted generation releases exactly when its last in-flight
/// request finishes draining on it.
///
/// # Two connections, one read-only (NRN-252)
///
/// A generation holds TWO connections to the same `cache.db`:
///
/// - [`cache`](Self::cache) — the request-facing READ connection. After NRN-252
///   the request path never writes through it (the freshness refresh moved off
///   it); it only serves reads. This read-only-in-practice invariant is the
///   precondition NRN-253 pools it under.
/// - [`write_cache`](Self::write_cache) — the WRITE connection, touched ONLY by
///   the writer thread's ops: the freshness-refresh op and the post-apply
///   cache-increment commit (NRN-252 / NRN-158). WAL mode makes the read
///   connection observe its committed rows across the connection boundary, so a
///   refresh (or increment) awaited before a read hands back fresh data.
pub(crate) struct Generation {
    /// Monotonic generation number (1 for the first open, incremented per
    /// reopen). Used to coalesce concurrent opens and to gate corruption/panic
    /// invalidation against the slot's floor.
    number: u64,
    /// `(dev, ino)` of `cache.db` at open; compared per-request for ground-shift.
    db_identity: (u64, u64),
    /// The index identity this generation was opened under; compared against the
    /// live config per-request to detect an index-relevant change.
    index_identity: IndexIdentity,
    /// Held-open handle to `cache.db` captured at open. Holding it keeps the
    /// inode meaningful for the generation's life (its fstat produced
    /// `db_identity`); never read again, so `_sentinel` documents intent and
    /// suppresses dead-field lints.
    _sentinel: File,
    /// The request-facing READ connection. `Arc<Mutex>` so a request can take an
    /// OWNED lock (`lock_arc`) held across the whole sync tool body, outliving a
    /// concurrent generation swap of the slot's current pointer. `parking_lot`
    /// (not `std`): its `ArcMutexGuard` is a safe owned guard, and a panic while a
    /// tool holds it is handled precisely by the handle's `Drop` (invalidation
    /// floor bump), not by std-mutex poisoning. Read-only in practice after
    /// NRN-252 — the freshness refresh writes through `write_cache` instead.
    cache: Arc<CacheMutex<Cache>>,
    /// The WRITE connection, a second connection to the same `cache.db` opened
    /// via the verification-skipping companion path (see
    /// `Cache::open_companion_verified`). Written through ONLY by the writer-queue
    /// ops — the freshness-refresh op ([`run_refresh_op`]) and the post-apply
    /// increment commit ([`run_increment_op`], NRN-252 / NRN-158) — which run
    /// serialized on the per-vault writer thread, so a plain `std::sync::Mutex` is
    /// always uncontended here; it exists only to satisfy `&mut` through the
    /// shared `Arc<Generation>`.
    write_cache: Mutex<Cache>,
    /// Coalescing state for freshness refreshes on this generation (NRN-252).
    /// Holds the single in-flight-or-queued [`RefreshTicket`] that arriving
    /// requesters may join, or `None` when no refresh is pending (or the pending
    /// one has already started its scan). Guarded so the join decision is atomic
    /// against an op's start transition — see [`arrive_refresh`](VaultContext::arrive_refresh).
    refresh_pending: Mutex<Option<Arc<RefreshTicket>>>,
    /// Test-only: total freshness-refresh executions that actually reached
    /// `index_incremental` on this generation. Drives the coalescing /
    /// arrival-correctness assertions (exactly-one-execution counts).
    #[cfg(test)]
    refresh_exec_count: AtomicU64,
    /// Test-only: a one-shot error the next refresh op returns INSTEAD of running
    /// `index_incremental`, so a test can drive a corruption-class refresh failure
    /// through the ticket and prove `note_tool_error` still classifies it.
    #[cfg(test)]
    inject_refresh_error: Mutex<Option<CacheError>>,
    /// Test-only: a one-shot gate the next refresh op waits on AFTER its start
    /// transition (started flag set, pending cleared) and BEFORE its scan, so a
    /// test can hold a refresh "in flight" while a new requester arrives.
    #[cfg(test)]
    refresh_gate: Mutex<Option<TestGate>>,
    /// Test-only: the `IndexReport` the most recent freshness refresh produced,
    /// captured so the NRN-158 acceptance test can assert an empty report (zero
    /// changes ⇒ no whole-vault rebuild) after a warm mutation committed its
    /// increment.
    #[cfg(test)]
    last_refresh_report: Mutex<Option<crate::cache::IndexReport>>,
    /// Test-only: a reusable gate the increment-commit op signals + waits on at
    /// EACH chunk boundary (after a chunk commits, before the next), so a test can
    /// observe intermediate committed state, interleave a liveness op, or turn the
    /// generation stale mid-commit — all without sleeps.
    #[cfg(test)]
    increment_gate: Mutex<Option<TestGate>>,
}

/// Marker strings a refresh ticket resolves to when its op never produced a
/// result — the queue dropped it on shutdown, or it panicked. Every waiter
/// surfaces these as an error (never a hang).
const REFRESH_QUEUE_SHUTDOWN: &str =
    "warm writer queue is shutting down; freshness refresh abandoned";
const REFRESH_PANICKED: &str = "warm writer queue panicked while running a freshness refresh";

/// Operator notes for a DEGRADED post-apply increment commit (NRN-252 / NRN-158).
/// The mutation already succeeded on disk, so these never fail the tool call —
/// they announce that the cache update was deferred and the next read's freshness
/// refresh will heal it (files remain the source of truth). Emitted on BOTH
/// surfaces: the daemon's own stderr and the per-request note buffer.
const INCREMENT_FAILED_NOTE: &str = "norn serve: post-apply cache increment failed; the cache update was deferred and the next read's refresh will heal it";
const INCREMENT_DROPPED_NOTE: &str = "norn serve: post-apply cache increment abandoned (generation evicted or queue shutdown); the next read's refresh will heal the cache";
const INCREMENT_PANICKED_NOTE: &str =
    "norn serve: post-apply cache increment panicked; the next read's refresh will heal the cache";

/// A coalescing ticket for ONE freshness-refresh execution on a generation's
/// write connection (NRN-252). Requesters that arrive before the refresh op
/// starts share one ticket — and thus one `index_incremental` execution —
/// blocking here on the condvar rather than each queuing a redundant refresh.
///
/// The ticket resolves on EVERY path — success, `LockTimeout`, other error, and
/// (via the submitter's backstop) queue-drop / panic — so a waiter never hangs.
/// Every waiter observes the true outcome CLASS: a failed refresh is `Failed`
/// for all of them (the first takes the concrete `CacheError`, later waiters
/// synthesize one), a `LockTimeout` is `LockContention` for all of them, and a
/// clean refresh is `Served` for all of them — no coalesced waiter ever mistakes
/// a failed or contended refresh for a served one.
struct RefreshTicket {
    state: Mutex<RefreshTicketState>,
    resolved: Condvar,
}

/// How a refresh op resolved, held on the shared ticket so EVERY coalesced
/// waiter reads the same outcome class. Making illegal states unrepresentable
/// (no `resolved` bool + `Option` result + `Option` reason triple) is what
/// prevents a later waiter from silently reading a failed refresh as served.
enum Resolution {
    /// No outcome delivered yet; waiters block on the condvar.
    Pending,
    /// The op never produced a result (queue drop / panic) — every waiter fails.
    Abandoned(&'static str),
    /// The op ran and delivered a terminal class — every waiter sees this class.
    Done(DoneClass),
}

/// The terminal class of a completed refresh, shared across all coalesced
/// waiters.
enum DoneClass {
    /// Clean refresh (or no changes) — `Served` for everyone.
    Ok,
    /// Timed out on the cache write lock — `LockContention` for everyone (each
    /// waiter emits its own both-surfaces note at its call site).
    LockTimeout,
    /// Non-timeout failure — `Failed` for everyone. The concrete `CacheError` is
    /// moved out by the FIRST waiter (so `note_tool_error` keeps classifying
    /// corruption on the untouched type); every later waiter synthesizes a
    /// [`CacheError::CoalescedRefreshFailed`] — it need not re-trigger corruption
    /// eviction, since the first waiter's propagation already bumped the floor
    /// and eviction is idempotent.
    Failed(Option<CacheError>),
}

struct RefreshTicketState {
    /// Flipped true (under the generation's `refresh_pending` lock, so an
    /// arriving requester's started-check and join are atomic against it) when
    /// the op begins on the writer thread. A started refresh may have begun
    /// before a later arrival, so late arrivals never join it — they submit a
    /// fresh op whose scan is guaranteed to start after their arrival.
    started: bool,
    /// The single source of truth for this ticket's outcome. `Pending` until the
    /// op (or the submitter's drop/panic backstop) resolves it; the transition
    /// out of `Pending` happens once and is idempotent, and every waiter then
    /// reads the SAME class off it.
    resolution: Resolution,
}

impl RefreshTicket {
    fn new() -> Arc<Self> {
        Arc::new(RefreshTicket {
            state: Mutex::new(RefreshTicketState {
                started: false,
                resolution: Resolution::Pending,
            }),
            resolved: Condvar::new(),
        })
    }

    /// Deliver the op's `index_incremental` result. Idempotent: the first
    /// delivery wins (a later backstop resolve is a no-op). The concrete result
    /// is classified ONCE here into a [`DoneClass`] every waiter shares.
    fn resolve_completed(&self, result: Result<(), CacheError>) {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if !matches!(st.resolution, Resolution::Pending) {
            return;
        }
        let class = match result {
            Ok(()) => DoneClass::Ok,
            Err(CacheError::LockTimeout) => DoneClass::LockTimeout,
            Err(other) => DoneClass::Failed(Some(other)),
        };
        st.resolution = Resolution::Done(class);
        self.resolved.notify_all();
    }

    /// Resolve the ticket as abandoned (queue drop / panic). Idempotent.
    fn resolve_abandoned(&self, why: &'static str) {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if !matches!(st.resolution, Resolution::Pending) {
            return;
        }
        st.resolution = Resolution::Abandoned(why);
        self.resolved.notify_all();
    }

    /// Block until resolved, then classify the outcome for the requester. Every
    /// waiter observes the same CLASS: `Ok` → `Served`, `LockTimeout` →
    /// `LockContention`, `Failed` → `Failed` (first waiter gets the concrete
    /// error, later waiters a synthesized one), `Abandoned` → `Abandoned`.
    fn wait(&self) -> RefreshOutcome {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        while matches!(st.resolution, Resolution::Pending) {
            st = self.resolved.wait(st).unwrap_or_else(|p| p.into_inner());
        }
        match &mut st.resolution {
            Resolution::Pending => unreachable!("loop exits only once resolved"),
            Resolution::Abandoned(why) => RefreshOutcome::Abandoned(why),
            Resolution::Done(DoneClass::Ok) => RefreshOutcome::Served,
            Resolution::Done(DoneClass::LockTimeout) => RefreshOutcome::LockContention,
            // First waiter takes the concrete error (corruption stays
            // classifiable); every later coalesced waiter still fails, with a
            // synthesized error — none ever reads this as served.
            Resolution::Done(DoneClass::Failed(err)) => {
                RefreshOutcome::Failed(err.take().unwrap_or(CacheError::CoalescedRefreshFailed))
            }
        }
    }

    fn is_started(&self) -> bool {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).started
    }

    fn mark_started(&self) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).started = true;
    }
}

/// How a coalesced freshness refresh resolved, as seen by a requester.
enum RefreshOutcome {
    /// The refresh completed cleanly (or nothing changed) — serve the read.
    Served,
    /// The refresh timed out on the cache write lock. Serve anyway, emitting the
    /// NRN-215 both-surfaces contention note.
    LockContention,
    /// The refresh failed with a non-timeout error; propagate it (its concrete
    /// `CacheError` is intact for corruption classification).
    Failed(CacheError),
    /// The op was dropped (queue shutdown) or panicked; fail this request.
    Abandoned(&'static str),
}

/// The result of arriving at a generation's coalesced refresh: either this
/// caller SUBMITTED the op (and holds the queue handle that backstops
/// drop/panic) or it JOINED an already-pending, not-yet-started op.
enum RefreshArrival {
    Submitted {
        ticket: Arc<RefreshTicket>,
        handle: Handle<()>,
    },
    Joined {
        ticket: Arc<RefreshTicket>,
    },
}

impl RefreshArrival {
    /// Block until the refresh resolves. The submitter waits on the queue handle
    /// first — so a dropped (shutdown) or panicked op still resolves the ticket
    /// rather than hanging every joined waiter — then reads the ticket; a joiner
    /// simply blocks on the shared ticket.
    fn wait(self, generation: &Generation) -> RefreshOutcome {
        match self {
            RefreshArrival::Joined { ticket } => ticket.wait(),
            RefreshArrival::Submitted { ticket, handle } => {
                match handle.wait() {
                    Outcome::Done(()) => {}
                    Outcome::Dropped => {
                        ticket.resolve_abandoned(REFRESH_QUEUE_SHUTDOWN);
                        clear_pending_if(generation, &ticket);
                    }
                    Outcome::Panicked => {
                        ticket.resolve_abandoned(REFRESH_PANICKED);
                        clear_pending_if(generation, &ticket);
                    }
                }
                ticket.wait()
            }
        }
    }

    /// Test accessor: the ticket this arrival is bound to.
    #[cfg(test)]
    fn ticket(&self) -> &Arc<RefreshTicket> {
        match self {
            RefreshArrival::Submitted { ticket, .. } | RefreshArrival::Joined { ticket } => ticket,
        }
    }
}

/// Test-only handshake gate installed on a generation to hold a writer-queue op
/// at a control point under test control (no sleeps). One shape serves both uses:
///
/// - the freshness-refresh gate (`refresh_gate`) is ONE-SHOT — the refresh op
///   waits on it once, AFTER its start transition (started flag set, pending
///   cleared) and BEFORE its scan, so a test can hold a refresh "in flight" while
///   a new requester arrives;
/// - the increment-commit gate (`increment_gate`) is REUSABLE — the commit op
///   hits it at EVERY chunk boundary (after a chunk commits, before the next), so
///   a test can step a multi-chunk commit one boundary at a time.
///
/// In both: `reached.recv()` unblocks when the op reaches the gate; the test
/// inspects state, then `release.send(())` lets the op proceed.
#[cfg(test)]
struct TestGate {
    /// Signalled by the op once it reaches the gate.
    reached: std::sync::mpsc::Sender<()>,
    /// The op blocks receiving here until the test releases it.
    release: std::sync::mpsc::Receiver<()>,
}

/// Clear a generation's pending-refresh slot IFF it still points at `ticket`.
/// Used by the submitter's drop/panic backstop so a superseded ticket never
/// lingers in the slot.
fn clear_pending_if(generation: &Generation, ticket: &Arc<RefreshTicket>) {
    let mut pending = generation
        .refresh_pending
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if pending.as_ref().is_some_and(|p| Arc::ptr_eq(p, ticket)) {
        *pending = None;
    }
}

/// The body of a freshness-refresh liveness op, run serialized on the writer
/// thread against the generation's WRITE connection (NRN-252).
///
/// It first performs the START TRANSITION under the generation's
/// `refresh_pending` lock — marking the ticket started and removing it from the
/// pending slot — so a concurrently-arriving requester either joins this ticket
/// BEFORE the transition (its arrival precedes this scan) or, seeing the slot
/// cleared, submits a fresh op AFTER (its arrival precedes THAT scan). Either
/// way a requester is only ever served by a refresh whose scan starts at or
/// after its arrival. It then runs `index_incremental` and resolves the ticket
/// with the concrete result.
fn run_refresh_op(generation: &Generation, vault_root: &Utf8Path, ticket: &Arc<RefreshTicket>) {
    // Start transition: mark started and clear the pending slot atomically under
    // the pending lock (lock order: pending → ticket state, matching the arrival
    // path, so the join decision cannot straddle it).
    {
        let mut pending = generation
            .refresh_pending
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        ticket.mark_started();
        if pending.as_ref().is_some_and(|p| Arc::ptr_eq(p, ticket)) {
            *pending = None;
        }
    }

    // Test-only: block "in flight" between the start transition and the scan.
    #[cfg(test)]
    {
        let gate = generation
            .refresh_gate
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(gate) = gate {
            let _ = gate.reached.send(());
            let _ = gate.release.recv();
        }
    }

    // Test-only: return an injected error instead of scanning, to drive a
    // refresh failure (e.g. corruption) through the ticket deterministically.
    #[cfg(test)]
    {
        let injected = generation
            .inject_refresh_error
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(err) = injected {
            ticket.resolve_completed(Err(err));
            return;
        }
    }

    #[cfg(test)]
    generation
        .refresh_exec_count
        .fetch_add(1, Ordering::Relaxed);

    let result = {
        let mut write_cache = generation
            .write_cache
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let report = write_cache.index_incremental(vault_root, &ChangeDetectOptions::default());
        // Test-only: capture the report so the NRN-158 acceptance test can assert
        // it is empty (zero changes ⇒ no whole-vault rebuild) after an increment.
        #[cfg(test)]
        if let Ok(r) = &report {
            *generation
                .last_refresh_report
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = Some(r.clone());
        }
        report.map(|_report| ())
    };
    ticket.resolve_completed(result);
}

/// The body of a post-apply cache-increment commit BULK op, run serialized on the
/// writer thread against the generation's WRITE connection (ADR 0013 Phase 2,
/// NRN-252 / NRN-158).
///
/// It is a chunked closure driving an ALREADY-PARSED [`IncrementCommit`] (the
/// whole-vault parse ran on the caller thread, off the writer thread — NRN-252
/// review): each invocation commits ONE file-coherent chunk (or the final global
/// links rewrite). Each chunk acquires and releases the WriteLock around its own
/// transaction, so external CLI processes and this daemon's liveness ops
/// interleave at boundaries. Returns [`ChunkOutcome::More`] while work remains,
/// [`ChunkOutcome::Done`] with the terminal `Result` on completion or the first
/// chunk error.
fn run_increment_chunk(
    generation: &Generation,
    vault_root: &Utf8Path,
    driver: &mut crate::cache::IncrementCommit,
    budget: std::time::Duration,
) -> ChunkOutcome<Result<(), CacheError>> {
    let mut write_cache = generation
        .write_cache
        .lock()
        .unwrap_or_else(|p| p.into_inner());

    let outcome = match write_cache.commit_increment_chunk(vault_root, driver, budget) {
        Ok(true) => ChunkOutcome::More,
        Ok(false) => ChunkOutcome::Done(Ok(())),
        Err(e) => ChunkOutcome::Done(Err(e)),
    };
    // Release the write connection BEFORE hitting the test gate, so a test can
    // inspect the read connection while the op is parked at the boundary.
    drop(write_cache);

    // Test-only: park at each chunk boundary (a chunk just committed) so a test
    // can step the commit deterministically. Never fires when no gate installed.
    #[cfg(test)]
    if matches!(outcome, ChunkOutcome::More) {
        let gate = generation
            .increment_gate
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(g) = gate.as_ref() {
            let _ = g.reached.send(());
            let _ = g.release.recv();
        }
    }

    outcome
}

/// The slice of warm-slot state a writer-queue generation-open op touches. It
/// lives behind an `Arc` shared by the [`WarmSlot`] and every submitted open op,
/// because the op runs `'static` on the writer thread and cannot borrow the slot
/// (NRN-252). Serialization by the queue — not a mutex guard — is what coalesces
/// concurrent opens now, so these fields only need interior mutability for the
/// reads and the single swap the serialized op performs.
struct SharedSlot {
    /// The current generation, `None` only before the first open. Never nulled in
    /// place afterward: a reopen swaps this pointer to N+1 and the old N drops
    /// with its last in-flight `Arc`.
    current: Mutex<Option<Arc<Generation>>>,
    /// Monotonic invalidation floor: a generation whose `number` is BELOW this is
    /// stale and must be reopened. Bumped by corruption ([`note_tool_error`]) and
    /// by a panic unwinding through a live cache guard — the two triggers with no
    /// filesystem/config signal for `ensure_current` to observe. An atomic so the
    /// per-request freshness probe reads it without taking any lock; a separate
    /// `Arc` (not just the enclosing `Arc<SharedSlot>`) so each handed-out cache
    /// guard can hold a lightweight handle whose panic-`Drop` bumps it.
    floor: Arc<AtomicU64>,
    /// Next generation number to assign (1 for the first open). Mutated only by the
    /// serialized open op, so queue serialization alone keeps it single-flight —
    /// the former `open` guard's coalescing role is now the writer thread's.
    next_number: Mutex<u64>,
    /// Total generation opens performed (cold start + every reopen). Test-only
    /// coalescing/single-flight assertion counter; incremented on every open.
    open_count: AtomicU64,
}

/// Warm-mode per-vault slot. The small fields are guarded by `std::sync::Mutex`
/// (NOT tokio), locked only briefly and NEVER across an `.await` (tool bodies are
/// sync); the cache connection's own lock lives inside [`Generation`].
struct WarmSlot {
    /// Generation-open state shared with the writer-queue open ops (see
    /// [`SharedSlot`]) — `current`, the invalidation `floor`, the generation
    /// counter, and the open count.
    shared: Arc<SharedSlot>,
    /// Fingerprint of the config file; independent of any generation, so it
    /// survives reopens.
    config_fp: Mutex<ConfigFingerprint>,
    /// The per-vault writer queue: the single serialization point for generation
    /// opens and freshness refreshes — subsuming the former `open` single-flight
    /// mutex — and, in a later commit, apply increments (ADR 0013, NRN-252). A
    /// stale [`ensure_current`](VaultContext::ensure_current) submits a
    /// generation-open LIVENESS op and blocks on it; the queue runs opens one at a
    /// time, so N concurrent stale callers coalesce to one open and the rest adopt.
    /// The per-request freshness refresh is likewise a LIVENESS op on this queue,
    /// executed on the generation's write connection and coalesced per generation.
    queue: WriterQueue,
}

/// Per-request scope (NRN-253): the state that must be private to ONE in-flight
/// request once the daemon no longer serializes tool bodies with `call_lock`.
///
/// Three things previously lived context-global (or slot-global) and were only
/// safe because `call_lock` guaranteed at most one request per vault ran at a
/// time; NRN-253 moves each into this per-request value, threaded explicitly to
/// the tool body alongside `&VaultContext`:
///
/// - **The bound config** (`config`). A request binds the current
///   `Arc<LoadedConfig>` at its boundary ([`VaultContext::begin_request`]) and
///   reads it via [`RequestScope::config`] for its whole life, so a concurrent
///   request's `begin_request` swapping the stored config cannot split-brain this
///   one between two reads (NRN-251's `(generation, config)` binding, now
///   structural).
/// - **The operator-note buffer** (`notes`). Notes a request produces (e.g. the
///   write-lock contention note) accumulate here and are drained by
///   `run_wrapped` into exactly this request's tool envelope — concurrent
///   requests can no longer interleave notes into each other's envelopes.
/// - **The bound generation** (`bound_generation`). Stamped by
///   [`query_cache_warm`](VaultContext::query_cache_warm) when the request binds
///   its generation, read by [`note_tool_error`](VaultContext::note_tool_error)
///   to key corruption invalidation off the generation THIS request used — not
///   whichever generation happens to be `current` when the error surfaces. Warm-
///   only; inert (stays 0) in cold mode. `0` means "no generation bound yet".
///
/// Created on the request's `spawn_blocking` thread but built with `Send`-safe
/// interior mutability (`Mutex` / `AtomicU64`) so nothing here constrains where
/// the scope may be constructed or handed across threads.
pub(crate) struct RequestScope {
    /// The `Arc<LoadedConfig>` bound at the request boundary; held for the whole
    /// request so config is stable across every read within it.
    config: Arc<LoadedConfig>,
    /// This request's operator notes, drained by `run_wrapped` into its envelope.
    notes: Mutex<Vec<String>>,
    /// The generation number this request bound in `query_cache_warm`
    /// (`0` = none bound). Read by `note_tool_error` for corruption attribution.
    bound_generation: AtomicU64,
}

// `LoadedConfig` is not `Debug`, so derive is out; a manual impl lets a request
// scope appear in test `Result` messages (e.g. `begin_request().expect_err(..)`)
// without printing the whole resolved config.
impl std::fmt::Debug for RequestScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestScope")
            .field("notes", &self.notes)
            .field("bound_generation", &self.bound_generation)
            .finish_non_exhaustive()
    }
}

impl RequestScope {
    /// Build a scope bound to `config`, with an empty note buffer and no
    /// generation bound yet.
    fn new(config: Arc<LoadedConfig>) -> Self {
        Self {
            config,
            notes: Mutex::new(Vec::new()),
            bound_generation: AtomicU64::new(0),
        }
    }

    /// The config bound at this request's boundary. Tool bodies read config
    /// through here (not `VaultContext::config`) so it stays request-stable.
    pub(crate) fn config(&self) -> Arc<LoadedConfig> {
        Arc::clone(&self.config)
    }

    /// Record an operator note for this request. Drained by `run_wrapped` into
    /// the tool envelope's `operator_notes` (NRN-215). A poisoned lock is
    /// recovered in place — a lost note is a strictly better failure mode than
    /// panicking the request.
    pub(crate) fn push_operator_note(&self, note: impl Into<String>) {
        self.notes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(note.into());
    }

    /// Drain and return the notes accumulated for this request. Called by
    /// `run_wrapped` immediately after the tool body, so the returned notes
    /// belong to exactly this request.
    pub(crate) fn take_operator_notes(&self) -> Vec<String> {
        std::mem::take(&mut *self.notes.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// Stamp the generation this request bound (warm mode). Written once, right
    /// after `ensure_current` returns in `query_cache_warm`.
    fn bind_generation(&self, number: u64) {
        self.bound_generation.store(number, Ordering::Release);
    }

    /// The generation this request bound (`0` = none). Read by `note_tool_error`.
    fn bound_generation(&self) -> u64 {
        self.bound_generation.load(Ordering::Acquire)
    }
}

/// Cold (stdio) vs warm (daemon) behavior for `query_cache`.
// The warm variant is naturally large (it owns the held cache state); exactly
// one `Mode` exists per long-lived `VaultContext`, so the size gap is irrelevant.
#[allow(clippy::large_enum_variant)]
enum Mode {
    Cold,
    // Constructed by `open_warm`, used by the unix-only `norn serve` daemon
    // (`src/serve/`); dead on non-unix builds where the daemon can't run.
    #[cfg_attr(not(unix), allow(dead_code))]
    Warm(WarmSlot),
}

/// Vault context for the MCP servers.
///
/// Holds a parsed [`LoadedConfig`] behind a `Mutex<Arc<..>>` (warm mode swaps in
/// a re-parsed config on a config edit; cold mode never mutates it) and a
/// [`Mode`] that selects the per-call cache strategy. In warm mode the cache
/// binding itself is NOT interior-mutable — it is an immutable `Arc<Generation>`
/// swapped as a whole. See the module docs for the full design.
pub(crate) struct VaultContext {
    /// Absolute path to the vault root, as passed via `--cwd`.
    pub(crate) vault_root: Utf8PathBuf,
    /// Parsed and compiled config. Behind a `Mutex<Arc<..>>` so warm mode can
    /// atomically swap in a re-parsed config without disturbing readers holding
    /// a cloned `Arc`. This is the config `Arc` a request binds at its boundary.
    config: Mutex<Arc<LoadedConfig>>,
    mode: Mode,
}

impl VaultContext {
    /// Open a COLD vault context (stdio `norn mcp`). Reads and parses the config
    /// once; fails fast if the config file exists but is unreadable/malformed.
    ///
    /// A missing config file is not an error — `load_config` returns
    /// `LoadedConfig::default()` when no `.norn/config.yaml` is found, so the
    /// server starts cleanly against unconfigured vaults.
    pub(crate) fn open(cwd: &Utf8Path, config_path: Option<&Utf8PathBuf>) -> Result<Self> {
        let config = load_config(&cwd.to_path_buf(), config_path)?;
        Ok(Self {
            vault_root: cwd.to_path_buf(),
            config: Mutex::new(Arc::new(config)),
            mode: Mode::Cold,
        })
    }

    /// Open a WARM vault context (daemon `norn serve`). Parses the config once at
    /// startup (from the DEFAULT location only — see below) and captures its
    /// fingerprint; the cache is opened lazily on the first `query_cache` call
    /// and then held open, verify-once, across requests.
    ///
    /// Warm mode is only ever constructed with the default config location: the
    /// daemon wire never carries a custom `--config` path, so this takes only
    /// `cwd` and hard-codes `config_path = None`. Config freshness is tracked
    /// against `<vault_root>/.norn/config.yaml` accordingly.
    // Used by the unix-only `norn serve` daemon (`src/serve/`); dead on non-unix
    // builds where the daemon can't run.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn open_warm(cwd: &Utf8Path) -> Result<Self> {
        let config = load_config(&cwd.to_path_buf(), None)?;
        let config_fp = fingerprint_config(&config_yaml_path(cwd))?;
        Ok(Self {
            vault_root: cwd.to_path_buf(),
            config: Mutex::new(Arc::new(config)),
            mode: Mode::Warm(WarmSlot {
                shared: Arc::new(SharedSlot {
                    current: Mutex::new(None),
                    floor: Arc::new(AtomicU64::new(0)),
                    // Generation numbers start at 1, so the counter's next value is 1.
                    next_number: Mutex::new(1),
                    open_count: AtomicU64::new(0),
                }),
                config_fp: Mutex::new(config_fp),
                // Label the writer thread with the vault root for debuggability.
                queue: WriterQueue::spawn(cwd.as_str()),
            }),
        })
    }

    /// Per-request entry seam (FIX-1): runs the warm pipeline's root-liveness
    /// (step 0) and config-freshness (step 1) once per request, BEFORE the tool
    /// body reads `config()` or opens the cache. No-op in cold mode (config is
    /// fixed for the process lifetime and the cache is opened fresh each call).
    ///
    /// Split out of `query_cache` so every tool — including the ones that bypass
    /// `query_cache` and build the graph index via `load_graph_index` — gets
    /// root-liveness + a fresh config each request, and so config is STABLE for
    /// the whole request: `query_cache` no longer swaps config mid-request,
    /// closing the split-brain window where one request could mix an old-config
    /// graph index with a new-config cache. A gone root surfaces here as the
    /// typed [`WarmContextError::RootGone`] the daemon downcasts to evict.
    ///
    /// Returns the request's [`RequestScope`] (NRN-253): a fresh, empty note
    /// buffer, no generation bound yet, and the config `Arc` bound at this
    /// boundary — the tool body threads it and `run_wrapped` drains its notes.
    /// A fresh scope per request is what makes per-request isolation structural:
    /// there is no shared buffer or slot cell to clear, so a prior request's
    /// notes / generation attribution cannot leak into this one.
    pub(crate) fn begin_request(&self) -> Result<RequestScope> {
        let Mode::Warm(slot) = &self.mode else {
            // Cold mode: config is fixed for the process lifetime, so the bound
            // config is simply the current one. No liveness/freshness steps.
            return Ok(RequestScope::new(self.config()));
        };

        // Step 0 — root liveness. A gone root is a typed, downcast-matchable
        // error so the daemon can evict this whole context.
        std::fs::canonicalize(self.vault_root.as_std_path()).map_err(|source| {
            anyhow::Error::new(WarmContextError::RootGone {
                root: self.vault_root.clone(),
                source,
            })
        })?;

        // Step 1 — config freshness. Swaps the stored config `Arc` on a change and
        // advances the fingerprint; a parse error fails this request without
        // advancing it. begin_request drops NO generation state: whether the
        // change is index-relevant (and so needs a new generation) is decided in
        // `ensure_current` by comparing the bound generation's index identity to
        // the new config, so a single path owns every reopen.
        self.refresh_config_warm(slot)?;

        // Bind the now-current config into the request scope, AFTER the freshness
        // swap, so the whole request reads one stable config `Arc` even if a later
        // concurrent request swaps the stored config out from under it (NRN-251).
        Ok(RequestScope::new(self.config()))
    }

    /// Corruption-eviction seam (FIX-3): inspect a failed tool's error chain and,
    /// in warm mode, invalidate the current generation when the failure is a
    /// SQLite corruption-class error (`DatabaseCorrupt` / `NotADatabase`). The
    /// next request then reopens through the single-flight
    /// [`ensure_current`](Self::ensure_current) path (integrity_check → detect →
    /// rebuild) — the same self-heal a one-shot CLI gets for free. No-op in cold
    /// mode (each call already opens + verifies a fresh cache).
    ///
    /// Invalidation bumps the slot's monotonic floor above the current
    /// generation's number rather than nulling the pointer in place, so eviction
    /// and reopen both flow through the one open path and any request still
    /// draining on the corrupt generation is undisturbed (it drops with its last
    /// `Arc`).
    ///
    /// Trust framing (ADR 0005): warm mode verifies integrity once and never
    /// re-runs integrity_check by design. That holds because corruption
    /// *surfaces as errors*, and this error-evict-reverify loop re-establishes
    /// trust on the next request. Silent wrong-data corruption that raises no
    /// error is outside SQLite's own detection model too, so it is not in scope.
    ///
    /// Invalidation keys off the SCOPE's bound generation — the generation THIS
    /// request bound in `query_cache_warm` — NOT `slot.current` (NRN-253). Under
    /// the retiring `call_lock` serialization the two coincide, but a corruption
    /// error on generation N must never invalidate a healthy generation N+1 that
    /// has since become current (the concurrent read pool makes that observable):
    /// mirrors how `WarmGuard::Drop` bumps the floor off its OWN generation's
    /// number rather than off whatever is current at drop time.
    pub(crate) fn note_tool_error(&self, scope: &RequestScope, err: &anyhow::Error) {
        let Mode::Warm(slot) = &self.mode else {
            return;
        };
        if is_sqlite_corruption(err) {
            let bound = scope.bound_generation();
            if bound != 0 {
                slot.shared.floor.fetch_max(bound + 1, Ordering::AcqRel);
            }
        }
    }

    /// The current STORED config. Locks briefly, clones the `Arc`, and releases —
    /// so a warm config hot-swap can proceed independently of callers still
    /// reading through an earlier `Arc`. A poisoned lock is recovered in place
    /// (the value is an immutable `Arc` snapshot, so there is nothing to evict)
    /// rather than panicking on every subsequent request.
    ///
    /// Tool bodies do NOT read config through here — they read the request's
    /// bound config via [`RequestScope::config`], so a concurrent request's
    /// config swap cannot split-brain them (NRN-253). This method is the internal
    /// snapshot seam `begin_request` binds into the scope, plus a test accessor
    /// for asserting the live stored config after a request.
    pub(crate) fn config(&self) -> Arc<LoadedConfig> {
        self.config
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Open a query cache for one tool call. Serves both modes with the same call
    /// shape (`let cache = ctx.query_cache()?;`) via a `Deref`/`DerefMut`-into-
    /// `Cache` handle, so tool code does not fork on mode.
    ///
    /// - Cold: opens a fresh [`Cache`] via `open_for_query` (integrity_check +
    ///   incremental refresh every call), exactly as before.
    /// - Warm: runs the verify-once + per-request self-heal pipeline (see module
    ///   docs) and hands out an owned guard into the bound generation's connection.
    pub(crate) fn query_cache(&self, scope: &RequestScope) -> Result<CacheHandle> {
        match &self.mode {
            Mode::Cold => {
                let config = scope.config();
                let cache = open_for_query(&self.vault_root, &config.index_options, false)?;
                Ok(CacheHandle::Owned(cache))
            }
            Mode::Warm(slot) => self.query_cache_warm(slot, scope),
        }
    }

    /// Build the [`GraphIndex`](crate::core::GraphIndex) for one tool call,
    /// reusing the warm connection when the daemon serves the request.
    ///
    /// The MCP tools that reconstruct a graph index rather than run a
    /// `query_cache` filter (validate, repair, set, edit, delete, move, rewrite,
    /// apply, new) call this. It is a thin composition over
    /// [`query_cache`](Self::query_cache) — ONE dispatch site, ONE pipeline run —
    /// followed by the cache reader's `load_graph_index`:
    ///
    /// - **Cold:** `query_cache` opens a fresh [`Cache`] via `open_for_query`
    ///   (integrity_check + incremental refresh), and the reader reconstructs the
    ///   index from it — the exact sequence `cache_cmd::load_graph_index` runs
    ///   for a direct `norn` CLI invocation, byte-for-byte.
    /// - **Warm:** `query_cache` runs the per-request self-heal pipeline
    ///   (ground-shift → reopen-if-absent → incremental freshness) against the
    ///   held-open connection, and the reader reconstructs the index from those
    ///   rows. `files.ignore` is applied at cache-build time
    ///   (`Cache::open_with_index`) in BOTH modes, so ignored docs are absent
    ///   from the rows just as in a cold fresh open — the resulting `GraphIndex`
    ///   is identical to the cold path on the same vault state (NRN-130).
    ///
    /// A tool that needs the cache handle AND the index must NOT call this plus
    /// `query_cache` separately — that runs the pipeline twice on two snapshots.
    /// Call `query_cache` once and build the index from that handle
    /// (`cache.load_graph_index()?`), as `vault.set` / `vault.edit` do.
    ///
    /// # Trust posture (ADR 0005)
    ///
    /// Warm mode extends the daemon's verify-once trade-off — previously only
    /// carried by the `query_cache` reads — to graph-index construction, which
    /// feeds mutation *planning*: `integrity_check` is paid at open (and on
    /// every self-heal reopen), not per call, so in-place same-inode corruption
    /// of `cache.db` is detected by open-time verification plus error-time
    /// eviction ([`note_tool_error`](Self::note_tool_error) invalidates the
    /// current generation on any SQLite corruption-class error, and the next
    /// request reopens and re-verifies), NOT by a per-call recheck the way a cold
    /// open would.
    /// The source of truth remains the Markdown files: a plan built from a
    /// corrupt index is caught by the apply-time snapshot checks or surfaces as
    /// an error that triggers the eviction path. Direct (non-daemon) invocations
    /// keep the full per-call verification.
    ///
    /// Config freshness / root-liveness (steps 0–1) already ran in
    /// [`begin_request`](Self::begin_request) at the per-request seam, exactly as
    /// for `query_cache`, so config is stable for the whole request.
    pub(crate) fn load_graph_index(&self, scope: &RequestScope) -> Result<crate::core::GraphIndex> {
        let cache = self.query_cache(scope)?;
        Ok(cache.load_graph_index()?)
    }

    /// The warm per-request pipeline (steps 2–4). See the module-level docs for
    /// the ordered rationale of each step.
    fn query_cache_warm(&self, slot: &WarmSlot, scope: &RequestScope) -> Result<CacheHandle> {
        // Steps 0–1 (root-liveness + config-freshness) already ran in
        // `begin_request` at the per-request seam, so config is stable here for
        // the whole request. Steps 2–3 (ground-shift + reopen-if-stale) are the
        // single-flight `ensure_current`, which returns the generation this
        // request binds for its whole body.
        let generation = self.ensure_current(slot, scope)?;

        // Record which generation THIS request bound into its OWN scope, before
        // doing any further work that could fail (the freshness refresh below, or
        // the tool body after this returns) — so a corruption error noted anywhere
        // downstream attributes to the generation this request actually used, not
        // whatever is current by the time `note_tool_error` runs (NRN-253).
        scope.bind_generation(generation.number);

        // Step 4 — freshness, split into PROBE then (only if stale) REFRESH
        // (NRN-253). Take an OWNED lock on the bound generation's READ connection,
        // held across the whole (sync) tool body. It outlives a concurrent
        // generation swap of the slot pointer because the guard owns its own
        // `Arc<Mutex<Cache>>` and the handle also holds the `Arc<Generation>`
        // (keeping the sentinel alive). Read-only after NRN-252.
        let read_guard = generation.cache.lock_arc();

        // Probe on THIS request thread against the read connection. Fresh → serve
        // on this very guard, touching neither the writer queue nor the write
        // connection: a concurrent read of an unchanged vault costs one stat
        // sweep, not a refresh op. Stale (or a probe error — treated
        // conservatively as stale, since the refresh is authoritative and would
        // re-surface any real fault) → route through the coalesced refresh. No
        // re-probe after: an arrival-correct refresh IS the freshness proof, the
        // same trust semantics the always-refresh pipeline carried.
        let fresh = matches!(
            StatSweepProbe.probe(&self.vault_root, &read_guard),
            Ok(Freshness::Fresh)
        );

        let guard = if fresh {
            read_guard
        } else {
            // Drop the read guard before refreshing: the refresh writes through
            // the generation's WRITE connection (WAL makes its committed rows
            // visible across the connection boundary), and releasing the read lock
            // keeps this request off the read connection while it waits — the
            // read-only-in-practice invariant NRN-253 pools under.
            drop(read_guard);
            match self.refresh_generation(slot, &generation) {
                RefreshOutcome::Served => {}
                RefreshOutcome::LockContention => {
                    // BOTH surfaces (NRN-215): the daemon's own stderr is its
                    // operational log — an operator tailing `norn serve` (or a
                    // log pipeline) keeps the contention signal, alongside the
                    // served markers — AND the per-request note buffer carries
                    // it to the caller: `run_wrapped` forwards it in the tool
                    // envelope and the routed CLI re-emits it on ITS stderr,
                    // byte-identical to a direct run.
                    self.note_both_surfaces(scope, crate::cache::LOCK_CONTENTION_NOTE);
                }
                // The concrete `CacheError` survives the ticket, so a
                // corruption-class failure remains classifiable by
                // `note_tool_error` downstream.
                RefreshOutcome::Failed(error) => return Err(error.into()),
                RefreshOutcome::Abandoned(msg) => return Err(anyhow::anyhow!(msg)),
            }
            // Re-take the read guard for the tool body now the refresh has landed.
            generation.cache.lock_arc()
        };

        Ok(CacheHandle::Warm(WarmGuard {
            number: generation.number,
            floor: Arc::clone(&slot.shared.floor),
            _generation: generation,
            guard,
        }))
    }

    /// Run the coalesced freshness refresh for `generation` and block on its
    /// outcome (NRN-252). A thin composition: [`arrive_refresh`](Self::arrive_refresh)
    /// decides join-or-submit, then [`RefreshArrival::wait`] blocks with the
    /// drop/panic backstop.
    fn refresh_generation(&self, slot: &WarmSlot, generation: &Arc<Generation>) -> RefreshOutcome {
        self.arrive_refresh(slot, generation).wait(generation)
    }

    /// Arrive at `generation`'s coalesced refresh WITHOUT blocking. Under the
    /// generation's `refresh_pending` lock (so the started-check and the join are
    /// atomic against an op's start transition), either join a pending,
    /// not-yet-started refresh or submit a fresh writer-queue liveness op and
    /// install it as the pending one. Returns the [`RefreshArrival`] the caller
    /// then blocks on.
    fn arrive_refresh(&self, slot: &WarmSlot, generation: &Arc<Generation>) -> RefreshArrival {
        let decision = {
            let mut pending = generation
                .refresh_pending
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            // Join only a pending refresh that has NOT started: its eventual scan
            // is then necessarily after this arrival. (An already-started op has
            // cleared itself from the slot, so this normally sees `None` there;
            // the started re-check is a belt-and-suspenders guard on the race.)
            let joinable = pending.as_ref().and_then(|t| {
                if t.is_started() {
                    None
                } else {
                    Some(Arc::clone(t))
                }
            });
            match joinable {
                Some(ticket) => Some(ticket),
                None => {
                    let ticket = RefreshTicket::new();
                    *pending = Some(Arc::clone(&ticket));
                    // Signal "submit" by returning None here and building the op
                    // below with this freshly-installed ticket.
                    return self.submit_refresh(slot, generation, ticket);
                }
            }
        };
        RefreshArrival::Joined {
            ticket: decision.expect("join path returns Some"),
        }
    }

    /// Build and submit a freshness-refresh liveness op for `ticket`, returning
    /// the [`RefreshArrival::Submitted`] handle. The op runs `'static` on the
    /// writer thread, so it captures owned / `Arc` state — never `&self`/`&slot`.
    fn submit_refresh(
        &self,
        slot: &WarmSlot,
        generation: &Arc<Generation>,
        ticket: Arc<RefreshTicket>,
    ) -> RefreshArrival {
        let gen_op = Arc::clone(generation);
        let ticket_op = Arc::clone(&ticket);
        let vault_root = self.vault_root.clone();
        let handle = slot
            .queue
            .submit_liveness(move || run_refresh_op(&gen_op, &vault_root, &ticket_op));
        RefreshArrival::Submitted { ticket, handle }
    }

    /// Commit the changed-file set from a just-succeeded WARM mutation as a
    /// CHUNKED cache-increment bulk op on the per-vault writer queue, AWAITED
    /// before returning (ADR 0013 Phase 2, NRN-252 / NRN-158). The tool returns
    /// with the cache already current, so the NEXT read's freshness refresh finds
    /// zero changes and pays neither a full detect scan nor a whole-vault rebuild.
    /// No-op in cold mode (no queue) and for an empty set.
    ///
    /// **Failure is degraded, never propagated.** The mutation ALREADY succeeded
    /// on disk, so an increment `Err` / `Dropped` / `Panicked` only means the
    /// cache update was deferred — the next read's refresh detects the change and
    /// heals it, with the files still the source of truth (trust preserved). The
    /// degrade emits the both-surfaces operator note; a corruption-class error
    /// still feeds [`note_tool_error`](Self::note_tool_error) so the generation is
    /// evicted and re-verified on the next request.
    pub(crate) fn commit_apply_increments(
        &self,
        scope: &RequestScope,
        changed_paths: &[Utf8PathBuf],
    ) {
        let Mode::Warm(slot) = &self.mode else {
            return; // cold: no queue, nothing to commit
        };
        if changed_paths.is_empty() {
            return;
        }
        // The generation this request bound (== current under `call_lock`). If
        // none is current, there is nothing to update — the next open covers it.
        let Some(generation) = slot
            .shared
            .current
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        else {
            return;
        };

        // Build the whole-vault parse (the IncrementCommit driver) HERE, on the
        // request's spawn_blocking thread (post-apply, under the mutation lock),
        // NOT on the writer thread (NRN-252 review). The parse is O(vault) and
        // was previously the bulk op's unbounded, non-preemptible first "chunk" —
        // defeating the ~50ms preemption bound a liveness refresh queued behind it
        // relies on. It is read-only over post-apply disk state and needs only the
        // generation's index config (alias_field / files.ignore), no cache
        // connection, so it belongs off the writer thread.
        let commit = match crate::cache::Cache::begin_increment_commit(
            &self.vault_root,
            changed_paths,
            generation.index_identity.alias_field.as_deref(),
            &generation.index_identity.ignore,
        ) {
            Ok(commit) => commit,
            Err(error) => {
                // The parse failed, but the mutation ALREADY landed on disk — so
                // degrade rather than fail the tool: the next read's refresh heals
                // the cache. A corruption-class parse error still evicts +
                // re-verifies the generation via `note_tool_error`.
                let err: anyhow::Error = error.into();
                self.note_tool_error(scope, &err);
                self.note_both_surfaces(scope, INCREMENT_FAILED_NOTE);
                return;
            }
        };

        let outcome = self
            .submit_increment_commit(slot, &generation, commit)
            .wait();
        match outcome {
            Outcome::Done(Ok(())) => {}
            Outcome::Done(Err(error)) => {
                // A corruption-class error must still evict + re-verify the
                // generation on the next request, exactly as a corruption error
                // surfaced from a tool body does (keys off the bound generation).
                let err: anyhow::Error = error.into();
                self.note_tool_error(scope, &err);
                self.note_both_surfaces(scope, INCREMENT_FAILED_NOTE);
            }
            Outcome::Dropped => self.note_both_surfaces(scope, INCREMENT_DROPPED_NOTE),
            Outcome::Panicked => self.note_both_surfaces(scope, INCREMENT_PANICKED_NOTE),
        }
    }

    /// Emit an operator note on BOTH surfaces (NRN-215 pattern): the daemon's own
    /// stderr keeps the signal for an operator tailing `norn serve`, and the
    /// per-request note buffer carries it to the caller (forwarded by
    /// `run_wrapped` and re-emitted on the routed CLI's stderr). The ONE shared
    /// shape for every both-surfaces note — the post-apply increment-deferred
    /// degrade notes and the freshness-refresh lock-contention note alike.
    fn note_both_surfaces(&self, scope: &RequestScope, note: &str) {
        eprintln!("{note}");
        scope.push_operator_note(note);
    }

    /// Submit the increment-commit bulk op for `generation`, driving the ALREADY
    /// PARSED `commit` chunk by chunk, and return its queue handle. The op runs
    /// `'static` on the writer thread, so it captures owned / `Arc` state; the
    /// whole-vault parse already happened on the caller thread (NRN-252 review),
    /// so the op only ever commits file-coherent chunks (each: lock → tx →
    /// commit) plus the final links rewrite — every chunk bounded, preemptible by
    /// a liveness op at its boundary.
    fn submit_increment_commit(
        &self,
        slot: &WarmSlot,
        generation: &Arc<Generation>,
        commit: crate::cache::IncrementCommit,
    ) -> Handle<Result<(), CacheError>> {
        let gen_op = Arc::clone(generation);
        let vault_root = self.vault_root.clone();
        let budget = crate::cache::increment_chunk_budget();
        // The prebuilt chunk-driver, moved into the op and advanced one chunk per
        // invocation.
        let mut driver = commit;
        let step = move || run_increment_chunk(&gen_op, &vault_root, &mut driver, budget);
        let still_valid = generation_still_current_guard(&slot.shared, generation.number);
        slot.queue.submit_bulk(step, Some(still_valid))
    }

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
    fn ensure_current(&self, slot: &WarmSlot, scope: &RequestScope) -> Result<Arc<Generation>> {
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

    /// Step 1 body. On a config-file change, re-parses and swaps the stored config
    /// `Arc` (advancing the fingerprint); an index-relevant change is NOT acted on
    /// here — `ensure_current` reopens when the bound generation's index identity
    /// no longer matches. On no change, touches nothing. On a parse error, returns
    /// `Err` WITHOUT advancing the fingerprint (so the next request retries),
    /// mirroring a direct CLI invocation.
    fn refresh_config_warm(&self, slot: &WarmSlot) -> Result<()> {
        let config_path = config_yaml_path(&self.vault_root);
        let new_fp = fingerprint_config(&config_path)?;

        let mut fp_guard = slot.config_fp.lock().unwrap_or_else(|p| p.into_inner());
        if *fp_guard == new_fp {
            return Ok(());
        }

        // Changed — re-parse. A parse error propagates and the fingerprint stays
        // stale, mirroring what a direct CLI invocation would do on this vault.
        let new_config = load_config(&self.vault_root.to_path_buf(), None)?;

        {
            let mut cfg = self.config.lock().unwrap_or_else(|p| p.into_inner());
            *cfg = Arc::new(new_config);
        }
        *fp_guard = new_fp;

        Ok(())
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

    /// Test-only: the warm slot's writer queue (panics off warm mode). Lets a test
    /// occupy the writer thread with a blocker so a later generation-open op stays
    /// queued.
    #[cfg(test)]
    fn warm_writer_queue(&self) -> &WriterQueue {
        match &self.mode {
            Mode::Warm(slot) => &slot.queue,
            Mode::Cold => panic!("warm_writer_queue called on a cold context"),
        }
    }

    /// Test-only: submit a generation-open op exactly as `ensure_current` does,
    /// returning the raw queue handle (not the mapped `Result`), so a test can
    /// model a caller blocked on the op while the queue shuts down under it.
    #[cfg(test)]
    fn submit_generation_open(&self) -> Handle<Result<Arc<Generation>>> {
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

    /// Test convenience: open a query cache under a fresh, single-use
    /// [`RequestScope`]. Production threads the request's scope from
    /// `begin_request` (so notes land in the request's envelope and the bound
    /// generation drives corruption attribution); unit tests that only inspect
    /// the returned cache don't need to hold that scope, so this hides the
    /// boilerplate. Tests that DO assert scope-bound behavior (note isolation,
    /// corruption attribution) thread an explicit scope instead.
    #[cfg(test)]
    pub(crate) fn query_cache_unscoped(&self) -> Result<CacheHandle> {
        let scope = RequestScope::new(self.config());
        self.query_cache(&scope)
    }

    /// Test convenience: the graph-index twin of [`query_cache_unscoped`].
    #[cfg(test)]
    pub(crate) fn load_graph_index_unscoped(&self) -> Result<crate::core::GraphIndex> {
        let scope = RequestScope::new(self.config());
        self.load_graph_index(&scope)
    }

    /// Test-only: arrive at `generation`'s coalesced refresh WITHOUT blocking,
    /// so a test can sequence multiple arrivals against a blocked writer and
    /// inspect the resulting join-or-submit decision.
    #[cfg(test)]
    fn test_arrive_refresh(&self, generation: &Arc<Generation>) -> RefreshArrival {
        match &self.mode {
            Mode::Warm(slot) => self.arrive_refresh(slot, generation),
            Mode::Cold => panic!("test_arrive_refresh called on a cold context"),
        }
    }

    /// Test-only: run one coalesced freshness refresh against the CURRENT
    /// generation and block on its outcome — the full arrive-then-wait path.
    #[cfg(test)]
    fn test_refresh_current(&self) -> RefreshOutcome {
        match &self.mode {
            Mode::Warm(slot) => {
                let generation = slot
                    .shared
                    .current
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone()
                    .expect("a current generation");
                self.refresh_generation(slot, &generation)
            }
            Mode::Cold => panic!("test_refresh_current called on a cold context"),
        }
    }

    /// Test-only: how many changes a `detect` scan reports against the CURRENT
    /// generation's READ connection. The NRN-158 acceptance test asserts 0 after a
    /// warm mutation committed its increment (⇒ the next refresh does no
    /// whole-vault rebuild).
    #[cfg(test)]
    pub(crate) fn detect_change_count(&self) -> usize {
        let generation = self.current_generation().expect("a current generation");
        let cache = generation.cache.lock();
        cache.detect_change_count(&self.vault_root)
    }

    /// Test-only: the `IndexReport` the current generation's most recent freshness
    /// refresh produced (`None` if none has run). An empty report (all-zero
    /// counts) means the refresh detected zero changes and did not rebuild.
    #[cfg(test)]
    pub(crate) fn last_refresh_report(&self) -> Option<crate::cache::IndexReport> {
        self.current_generation().and_then(|g| {
            g.last_refresh_report
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        })
    }

    /// Test-only: install a per-chunk gate on `generation`'s increment op and hand
    /// back the test's `(reached_rx, release_tx)` ends. The op signals `reached`
    /// after each chunk commits and blocks on `release`, so a test steps a
    /// multi-chunk commit one boundary at a time.
    #[cfg(test)]
    pub(crate) fn install_increment_gate(
        &self,
        generation: &Arc<Generation>,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        *generation
            .increment_gate
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(TestGate {
            reached: reached_tx,
            release: release_rx,
        });
        (reached_rx, release_tx)
    }

    /// Test-only: submit an increment-commit bulk op for `generation` exactly as
    /// [`commit_apply_increments`](Self::commit_apply_increments) does, returning
    /// the raw queue handle (not awaited), so a test can drive the gate and
    /// inspect intermediate state before the op resolves.
    #[cfg(test)]
    pub(crate) fn test_submit_increment_commit(
        &self,
        generation: &Arc<Generation>,
        changed_paths: &[Utf8PathBuf],
    ) -> Handle<Result<(), CacheError>> {
        match &self.mode {
            Mode::Warm(slot) => {
                // Mirror `commit_apply_increments`: parse on THIS (test) thread,
                // then submit the prebuilt driver.
                let commit = crate::cache::Cache::begin_increment_commit(
                    &self.vault_root,
                    changed_paths,
                    generation.index_identity.alias_field.as_deref(),
                    &generation.index_identity.ignore,
                )
                .expect("test increment parse should succeed");
                self.submit_increment_commit(slot, generation, commit)
            }
            Mode::Cold => panic!("test_submit_increment_commit called on a cold context"),
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

/// Path to a vault's default config file, `<vault_root>/.norn/config.yaml`.
fn config_yaml_path(vault_root: &Utf8Path) -> Utf8PathBuf {
    vault_root.join(".norn/config.yaml")
}

/// Does this error chain carry a SQLite corruption-class failure
/// (`DatabaseCorrupt` / `NotADatabase`)? Drives warm mode's error-triggered
/// eviction (FIX-3). The rusqlite error is often wrapped (e.g. in `CacheError`),
/// so we walk the whole `anyhow` chain and downcast each cause.
fn is_sqlite_corruption(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<rusqlite::Error>()
            .and_then(|e| e.sqlite_error_code())
            .is_some_and(|code| {
                matches!(
                    code,
                    rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
                )
            })
    })
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
fn generation_is_fresh(
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
fn generation_still_current_guard(shared: &Arc<SharedSlot>, number: u64) -> ValidityGuard {
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
fn open_or_adopt(
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
fn map_open_outcome(outcome: Outcome<Result<Arc<Generation>>>) -> Result<Arc<Generation>> {
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
fn open_generation(
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
    // O(db-size) integrity_check (see `Cache::open_companion_verified`). Only the
    // writer thread's freshness refresh writes through it; the request-facing
    // `cache` above stays read-only (NRN-252).
    let write_cache = Cache::open_companion_verified(
        vault_root,
        opts.alias_field.as_deref(),
        &opts.ignore,
        &opts.resolved_index_set,
        &opts.resolved_index_set_hash,
    )?;

    // Companion inode reconciliation (NRN-252 review). The sentinel pins the
    // primary inode from being FREED, but an fd does not pin the PATH from being
    // REPLACED. The companion above opened BY PATH, so an external unlink+recreate
    // (`cache clear`) in the window between the primary open and this open would
    // bind the write connection to a DIFFERENT inode than the read connection: a
    // split-brain generation whose writes land on an inode reads never see, healed
    // only on the NEXT request's ground-shift check. Stat the path now and compare
    // against the live post-primary inode; on mismatch, fail the generation open
    // so the single-flight caller errors and the next request retries fresh
    // against one consistent inode. (Comparing against the live `read_identity`,
    // NOT the sentinel `db_identity`, is what keeps an ordinary rebuild-on-open —
    // which moves the inode legitimately, read and write both landing on the new
    // one — from tripping a false positive.) A swap AFTER this check is caught by
    // the per-request ground-shift check, exactly as today.
    let companion_identity = device_inode(&std::fs::metadata(db_path.as_std_path())?);
    if companion_identity != read_identity {
        anyhow::bail!(
            "cache.db at {db_path} was swapped between the read and write connection \
             opens (identity {read_identity:?} → {companion_identity:?}); failing this \
             generation open so the next request retries on a consistent inode"
        );
    }

    Ok(Generation {
        number,
        db_identity,
        index_identity: IndexIdentity::from_config(config),
        _sentinel: sentinel,
        cache: Arc::new(CacheMutex::new(cache)),
        write_cache: Mutex::new(write_cache),
        refresh_pending: Mutex::new(None),
        #[cfg(test)]
        refresh_exec_count: AtomicU64::new(0),
        #[cfg(test)]
        inject_refresh_error: Mutex::new(None),
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
fn ensure_cache_dir(cache_dir: &Utf8Path) -> Result<()> {
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

/// A cache handle handed out by [`VaultContext::query_cache`], serving both
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

/// An owned guard into the warm-mode bound generation's held-open `Cache`.
///
/// It keeps the whole `Arc<Generation>` alive (so the sentinel fd + connection
/// stay open for the request even if a concurrent open swaps the slot's current
/// pointer to a newer generation) and holds an owned `parking_lot` lock into that
/// generation's connection across the entire (sync) tool body.
///
/// `Drop` carries the generational replacement for std-mutex poison recovery: if
/// the guard is dropped while the thread is PANICKING (a tool body panicked
/// mid-work, possibly mid-mutation), it bumps the slot's invalidation floor above
/// this generation's number, so the next request reopens through
/// [`ensure_current`](VaultContext::ensure_current) and re-verifies integrity —
/// exactly the trust-restoring self-heal the old poison-evict path gave, now
/// routed through the one open path. On a normal drop it does nothing.
pub(crate) struct WarmGuard {
    /// This generation's number, compared against `floor` on a panic-drop.
    number: u64,
    /// Shared handle to the slot's invalidation floor, bumped on a panic-drop.
    floor: Arc<AtomicU64>,
    /// Owned lock into the generation's connection, held across the tool body.
    /// It owns its own clone of the generation's `Arc<Mutex<Cache>>`, so the
    /// connection stays alive independently of `_generation`.
    guard: ArcMutexGuard<parking_lot::RawMutex, Cache>,
    /// Keeps the bound generation (sentinel fd + identity) alive for the whole
    /// request, so a concurrent open swapping the slot's current pointer to a
    /// newer generation cannot close this request's sentinel out from under it.
    _generation: Arc<Generation>,
}

impl Drop for WarmGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.floor.fetch_max(self.number + 1, Ordering::AcqRel);
        }
    }
}

impl Deref for WarmGuard {
    type Target = Cache;
    fn deref(&self) -> &Cache {
        &self.guard
    }
}

impl DerefMut for WarmGuard {
    fn deref_mut(&mut self) -> &mut Cache {
        &mut self.guard
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use std::sync::mpsc;
    use tempfile::TempDir;

    /// Build a minimal temp vault with a few seeded docs.
    fn make_seeded_vault() -> (TempDir, Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-ctx-unit-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();

        std::fs::write(
            root.join("alpha.md"),
            "---\ntype: note\nstatus: active\n---\nAlpha body\n",
        )
        .unwrap();
        std::fs::write(
            root.join("beta.md"),
            "---\ntype: task\nstatus: backlog\n---\nBeta body\n",
        )
        .unwrap();
        std::fs::write(
            root.join("gamma.md"),
            "---\ntype: log\nstatus: done\n---\nGamma body\n",
        )
        .unwrap();

        (tmp, root)
    }

    /// Does a `warm_marker` TEMP TABLE exist on the handed-out connection? TEMP
    /// tables are per-connection, so this is a same-connection probe: present
    /// ⇒ the connection was reused; gone ⇒ it was reopened.
    fn marker_present(cache: &Cache) -> bool {
        let n: i64 = cache
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_temp_master WHERE type='table' AND name='warm_marker'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        n == 1
    }

    fn create_marker(cache: &Cache) {
        cache
            .conn()
            .execute("CREATE TEMP TABLE warm_marker (x INTEGER)", [])
            .unwrap();
    }

    fn doc_count(cache: &Cache) -> i64 {
        cache
            .conn()
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .unwrap()
    }

    fn write_config(root: &Utf8Path, body: &str) {
        let dir = root.join(".norn");
        std::fs::create_dir_all(dir.as_std_path()).unwrap();
        std::fs::write(dir.join("config.yaml").as_std_path(), body).unwrap();
    }

    // ---- Cold-mode regression tests (unchanged behavior) --------------------

    #[test]
    fn open_succeeds_and_exposes_vault_root() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("VaultContext::open should succeed");
        assert_eq!(
            ctx.vault_root, root,
            "vault_root should match the cwd passed in"
        );
    }

    #[test]
    fn open_without_config_file_yields_default_config() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open should succeed without config");
        // Default config has no alias_field configured.
        assert!(
            ctx.config().index_options.alias_field.is_none(),
            "default config should have no alias_field, got {:?}",
            ctx.config().index_options.alias_field
        );
    }

    #[test]
    fn open_with_config_propagates_alias_field() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-ctx-alias-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_config(&root, "links:\n  alias_field: aliases\n");

        let ctx = VaultContext::open(&root, None).expect("open with config should succeed");
        assert_eq!(
            ctx.config().index_options.alias_field.as_deref(),
            Some("aliases"),
            "alias_field should propagate from config"
        );
    }

    #[test]
    fn query_cache_returns_usable_cache_and_indexes_seeded_docs() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open should succeed");

        let cache = ctx
            .query_cache_unscoped()
            .expect("query_cache should return Ok");

        // Count documents via direct SQL — the cache must have indexed the
        // 3 seeded docs during the per-call freshness check inside open_for_query.
        assert_eq!(
            doc_count(&cache),
            3,
            "cache should contain exactly 3 seeded documents"
        );
    }

    #[test]
    fn query_cache_reflects_vault_changes_on_subsequent_calls() {
        let (tmp, root) = make_seeded_vault();

        let ctx = VaultContext::open(&root, None).expect("open should succeed");

        // First call — 3 docs.
        {
            let cache = ctx
                .query_cache_unscoped()
                .expect("first query_cache call should succeed");
            assert_eq!(doc_count(&cache), 3, "initial count should be 3");
        }

        // Write a fourth document to the vault.
        std::fs::write(
            tmp.path().join("delta.md"),
            "---\ntype: note\nstatus: active\n---\nDelta body\n",
        )
        .unwrap();

        // Second call — per-call freshness check must pick up the new doc.
        {
            let cache = ctx
                .query_cache_unscoped()
                .expect("second query_cache call should succeed");
            assert_eq!(
                doc_count(&cache),
                4,
                "per-call freshness check must index the new document"
            );
        }
    }

    // ---- Warm-mode tests ----------------------------------------------------

    /// Warm reuse / verify-once: two sequential warm calls share one connection.
    /// A TEMP TABLE created through the first guard is still visible through the
    /// second (TEMP tables are per-connection), and the captured `(dev, ino)`
    /// identity is unchanged.
    #[test]
    fn warm_reuses_one_connection_across_calls() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm should succeed");

        {
            ctx.begin_request().expect("begin_request");
            let cache = ctx.query_cache_unscoped().expect("first warm query_cache");
            create_marker(&cache);
        }
        let id1 = ctx.warm_db_identity();
        assert!(id1.is_some(), "warm state should be held after first call");

        {
            ctx.begin_request().expect("begin_request");
            let cache = ctx.query_cache_unscoped().expect("second warm query_cache");
            assert!(
                marker_present(&cache),
                "TEMP table must survive ⇒ same connection reused (verify-once)"
            );
        }
        let id2 = ctx.warm_db_identity();
        assert_eq!(id1, id2, "cache identity must be stable across warm calls");
    }

    /// Warm freshness: a doc added between calls appears in the second call.
    #[test]
    fn warm_reflects_vault_changes_on_subsequent_calls() {
        let (tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm should succeed");

        {
            ctx.begin_request().expect("begin_request");
            let cache = ctx.query_cache_unscoped().expect("first warm query_cache");
            assert_eq!(doc_count(&cache), 3, "initial count should be 3");
        }

        std::fs::write(
            tmp.path().join("delta.md"),
            "---\ntype: note\nstatus: active\n---\nDelta body\n",
        )
        .unwrap();

        {
            ctx.begin_request().expect("begin_request");
            let cache = ctx.query_cache_unscoped().expect("second warm query_cache");
            assert_eq!(
                doc_count(&cache),
                4,
                "warm per-request freshness must index the new document"
            );
        }
    }

    /// Ground-shift: deleting the cache dir out from under a live warm context
    /// forces a reopen — the TEMP-table marker is gone, the identity changed, and
    /// the rebuilt cache still serves the vault.
    #[test]
    fn warm_self_heals_when_cache_db_disappears() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm should succeed");

        {
            ctx.begin_request().expect("begin_request");
            let cache = ctx.query_cache_unscoped().expect("first warm query_cache");
            create_marker(&cache);
            assert_eq!(doc_count(&cache), 3);
        }
        let _id1 = ctx.warm_db_identity().expect("state held after first call");

        // Simulate `norn cache clear` / manual rm under a live daemon: remove the
        // whole cache dir (db + WAL + SHM). POSIX keeps the old inode alive
        // through the held connection, so only the ground-shift check catches it.
        let (_canonical, cache_dir) = cache_dir_for(&root).expect("cache_dir_for");
        std::fs::remove_dir_all(cache_dir.as_std_path()).expect("remove cache dir");

        {
            ctx.begin_request().expect("begin_request");
            let cache = ctx
                .query_cache_unscoped()
                .expect("second warm query_cache after clear");
            assert!(
                !marker_present(&cache),
                "TEMP table must be gone ⇒ the connection was reopened"
            );
            assert_eq!(
                doc_count(&cache),
                3,
                "reopened cache must be rebuilt against the live vault"
            );
        }
        // Warm state must be re-established after the heal. Do NOT assert
        // id1 != id2: eviction closes the old sentinel/connection, freeing the
        // old inode, and inode-recycling filesystems (ext4) routinely hand the
        // recreated cache.db the same (dev, ino) — observed on Linux CI. That
        // recycling never defeats detection in production: while the old file
        // is still *held*, its inode stays allocated, so a recreated file
        // cannot collide until after the ground-shift check has already fired.
        // The TEMP-table assertion above is the reopen proof.
        let _id2 = ctx.warm_db_identity().expect("state held after reopen");
    }

    /// Root vanish: removing the vault root between calls yields the typed
    /// `WarmContextError::RootGone` (downcast-asserted).
    #[test]
    fn warm_root_vanish_returns_typed_error() {
        // Own the tempdir explicitly so we can delete it mid-test without the
        // TempDir guard double-freeing on drop.
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-ctx-rootgone-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(root.join("alpha.md"), "---\ntype: note\n---\nAlpha body\n").unwrap();

        let ctx = VaultContext::open_warm(&root).expect("open_warm should succeed");
        {
            ctx.begin_request().expect("begin_request");
            let cache = ctx.query_cache_unscoped().expect("first warm query_cache");
            assert_eq!(doc_count(&cache), 1);
        }

        std::fs::remove_dir_all(root.as_std_path()).expect("remove vault root");

        // Root-liveness moved to the per-request seam (FIX-1), so the typed
        // RootGone now surfaces from begin_request, not query_cache.
        let err = ctx
            .begin_request()
            .expect_err("begin_request must fail once the root is gone");
        match err.downcast_ref::<WarmContextError>() {
            Some(WarmContextError::RootGone { root: r, .. }) => {
                assert_eq!(r, &root, "RootGone should carry the vault root");
            }
            other => panic!("expected WarmContextError::RootGone, got {other:?}"),
        }
    }

    /// A `files.ignore` change is index-relevant: it changes which documents are
    /// in the graph, so the warm cache must REOPEN (marker gone) and the
    /// newly-ignored docs must be purged on the reopen's refresh (NRN-117).
    #[test]
    fn warm_files_ignore_change_reopens_and_purges() {
        let (_tmp, root) = make_seeded_vault();
        // A doc under an ignorable subdir, indexed on the first open.
        std::fs::create_dir_all(root.join("Archive").as_std_path()).unwrap();
        std::fs::write(
            root.join("Archive/old.md"),
            "---\ntype: note\n---\nArchived\n",
        )
        .unwrap();
        let ctx = VaultContext::open_warm(&root).expect("open_warm should succeed");

        let archived_count = |cache: &Cache| -> i64 {
            cache
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM documents WHERE path LIKE 'Archive/%'",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        };

        {
            ctx.begin_request().expect("begin_request");
            let cache = ctx.query_cache_unscoped().expect("first warm query_cache");
            create_marker(&cache);
            assert_eq!(
                archived_count(&cache),
                1,
                "archived doc indexed before ignore"
            );
        }

        write_config(&root, "files:\n  ignore:\n    - \"Archive/**\"\n");

        {
            ctx.begin_request().expect("begin_request");
            let cache = ctx
                .query_cache_unscoped()
                .expect("second warm query_cache after files.ignore change");
            assert!(
                !marker_present(&cache),
                "files.ignore change must reopen the cache"
            );
            assert_eq!(
                archived_count(&cache),
                0,
                "newly-ignored doc must be purged after reopen"
            );
        }
        assert_eq!(
            ctx.config().index_options.ignore,
            vec!["Archive/**".to_string()],
            "config() must reflect the new files.ignore"
        );
    }

    /// A genuinely non-index config change (here `validate.ignore`, which is
    /// validation-scoped and feeds neither the resolved index set nor
    /// `files.ignore`) is hot-swapped into `config()` WITHOUT reopening the
    /// cache (the marker survives).
    #[test]
    fn warm_non_index_config_change_swaps_without_reopen() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm should succeed");

        {
            ctx.begin_request().expect("begin_request");
            let cache = ctx.query_cache_unscoped().expect("first warm query_cache");
            create_marker(&cache);
        }

        write_config(&root, "validate:\n  ignore:\n    - \"logs/**\"\n");

        {
            ctx.begin_request().expect("begin_request");
            let cache = ctx
                .query_cache_unscoped()
                .expect("second warm query_cache after non-index config change");
            assert!(
                marker_present(&cache),
                "non-index config change must NOT reopen the cache"
            );
        }
        assert_eq!(
            ctx.config().validate.ignore,
            vec!["logs/**".to_string()],
            "config() must reflect the new validate.ignore"
        );
    }

    /// Config reopen (index-relevant): adding `links.alias_field` between calls
    /// reopens the cache (marker gone) and the new alias field is live in
    /// `config().index_options`.
    #[test]
    fn warm_index_relevant_config_change_reopens() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm should succeed");

        {
            ctx.begin_request().expect("begin_request");
            let cache = ctx.query_cache_unscoped().expect("first warm query_cache");
            create_marker(&cache);
        }
        assert!(ctx.config().index_options.alias_field.is_none());

        // Index-relevant: alias_field feeds Cache::open_with_index.
        write_config(&root, "links:\n  alias_field: aliases\n");

        {
            ctx.begin_request().expect("begin_request");
            let cache = ctx
                .query_cache_unscoped()
                .expect("second warm query_cache after index-relevant config change");
            assert!(
                !marker_present(&cache),
                "index-relevant config change must reopen the cache"
            );
        }
        assert_eq!(
            ctx.config().index_options.alias_field.as_deref(),
            Some("aliases"),
            "config() must reflect the new alias_field"
        );
    }

    /// Config parse error: invalid YAML between calls fails that request without
    /// advancing the fingerprint; fixing the YAML lets the next request succeed
    /// and see the new config.
    #[test]
    fn warm_config_parse_error_fails_then_recovers() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm should succeed");

        // First call — clean, no config file yet.
        {
            ctx.begin_request().expect("begin_request");
            let cache = ctx.query_cache_unscoped().expect("first warm query_cache");
            assert_eq!(doc_count(&cache), 3);
        }

        // Invalid YAML (unterminated flow sequence). Config freshness moved to
        // the seam (FIX-1), so the parse error surfaces from begin_request.
        write_config(&root, "links:\n  alias_field: [unterminated\n");
        let err = ctx
            .begin_request()
            .expect_err("invalid config must fail this request");
        // Not a RootGone — it is a plain config parse error.
        assert!(
            err.downcast_ref::<WarmContextError>().is_none(),
            "parse error should not surface as WarmContextError"
        );

        // Fix the YAML — the next request must retry (fingerprint was not
        // advanced past the broken file) and succeed with the new config live.
        write_config(&root, "links:\n  alias_field: fixed\n");
        {
            ctx.begin_request().expect("begin_request");
            let cache = ctx
                .query_cache_unscoped()
                .expect("query_cache must succeed once the config is valid again");
            assert_eq!(doc_count(&cache), 3);
        }
        assert_eq!(
            ctx.config().index_options.alias_field.as_deref(),
            Some("fixed"),
            "config() must reflect the repaired config"
        );
    }

    /// An existing-but-unreadable config (`chmod 000`) must fail the request,
    /// distinctly from an absent config — a direct CLI invocation on the same
    /// vault would also fail to read it, and the daemon must not silently keep
    /// serving stale/default config instead. Restoring readability lets the
    /// next request retry and pick up the live config.
    #[test]
    #[cfg(unix)]
    fn warm_unreadable_config_fails_request_then_recovers() {
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;

        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm should succeed");
        write_config(&root, "links:\n  alias_field: original\n");
        let config_path = config_yaml_path(&root);

        // Baseline: readable config, request succeeds.
        ctx.begin_request()
            .expect("begin_request with readable config");

        // Make the config unreadable.
        std::fs::set_permissions(config_path.as_std_path(), Permissions::from_mode(0o000)).unwrap();

        // Probe: if we can still open the file for reading, we're root or the
        // filesystem doesn't enforce permissions — skip the assertion rather
        // than false-failing.
        if std::fs::File::open(config_path.as_std_path()).is_ok() {
            std::fs::set_permissions(config_path.as_std_path(), Permissions::from_mode(0o644))
                .unwrap();
            return;
        }

        let err = ctx
            .begin_request()
            .expect_err("unreadable config must fail this request, not read as absent");
        assert!(
            err.downcast_ref::<WarmContextError>().is_none(),
            "an unreadable config should not surface as WarmContextError"
        );

        // Restore readability, then rewrite with new content — the next
        // request must retry (fingerprint was not advanced past the
        // unreadable state) and succeed with the new config live.
        std::fs::set_permissions(config_path.as_std_path(), Permissions::from_mode(0o644)).unwrap();
        write_config(&root, "links:\n  alias_field: restored\n");
        ctx.begin_request()
            .expect("begin_request must succeed once the config is readable again");
        assert_eq!(
            ctx.config().index_options.alias_field.as_deref(),
            Some("restored"),
            "config() must reflect the new config once readable again"
        );
    }

    // ---- Warm graph-index build (NRN-130) -----------------------------------

    /// Serialize a `GraphIndex` to a canonical JSON value for structural
    /// comparison. `GraphIndex` derives `Serialize` but not `PartialEq`, and
    /// every section the reader loads is deterministically ordered (documents
    /// `ORDER BY path`; links and diagnostics `ORDER BY rowid`; headings by
    /// source position; block_ids lexicographic), so the serialized form is an
    /// order-stable structural fingerprint.
    fn index_json(index: &crate::core::GraphIndex) -> serde_json::Value {
        serde_json::to_value(index).expect("GraphIndex serializes")
    }

    /// Equivalence (the load-bearing NRN-130 invariant): a WARM graph-index build
    /// (reusing the daemon's held connection, verify-once) must produce a
    /// structurally identical `GraphIndex` to the COLD fresh-open path
    /// (`cache_cmd::load_graph_index`, integrity_check every call) on the same
    /// vault state.
    ///
    /// The fingerprint comparison alone cannot catch a reader that silently
    /// skips a whole section (both sides run the SAME reader), so the cold
    /// build is first pinned against explicit fixture expectations: document
    /// count, the resolved wikilink edges, a non-empty headings section, and
    /// the block id — a silently-dropped table fails here regardless of
    /// warm/cold parity.
    #[test]
    fn warm_load_graph_index_matches_cold() {
        use crate::core::LinkStatus;

        let (_tmp, root) = make_seeded_vault();
        // Give alpha a heading, two resolvable wikilinks, and a block id so the
        // comparison covers every section the reader reconstructs.
        std::fs::write(
            root.join("alpha.md"),
            "---\ntype: note\nstatus: active\n---\n# Alpha\n\nAlpha links to [[beta]] and [[gamma]]. ^b1\n",
        )
        .unwrap();

        // Cold: the exact entry point a direct CLI invocation / cold MCP uses.
        let cold_config = load_config(&root.to_path_buf(), None).expect("load_config");
        let cold = crate::cache_cmd::load_graph_index(&root, &cold_config.index_options, false)
            .expect("cold load_graph_index");

        // Pin the cold build against the fixture (reader-omission guard).
        assert_eq!(cold.documents.len(), 3, "three seeded docs");
        let alpha = cold
            .documents
            .iter()
            .find(|d| d.path == "alpha.md")
            .expect("alpha.md present");
        let resolved: Vec<&str> = alpha
            .links
            .iter()
            .filter(|l| l.status == LinkStatus::Resolved)
            .filter_map(|l| l.resolved_path.as_deref().map(|p| p.as_str()))
            .collect();
        assert_eq!(
            resolved,
            vec!["beta.md", "gamma.md"],
            "alpha's wikilink edges must resolve to beta.md and gamma.md"
        );
        assert_eq!(alpha.headings.len(), 1, "alpha has exactly one heading");
        assert_eq!(alpha.headings[0].text, "Alpha");
        assert_eq!(
            alpha.block_ids,
            vec!["b1".to_string()],
            "alpha's block id must round-trip through the cache"
        );

        // Warm: through the daemon context, reusing the held connection.
        let ctx = VaultContext::open_warm(&root).expect("open_warm");
        ctx.begin_request().expect("begin_request");
        let warm = ctx
            .load_graph_index_unscoped()
            .expect("warm load_graph_index");

        assert_eq!(
            index_json(&warm),
            index_json(&cold),
            "warm graph-index build must be structurally identical to the cold build"
        );
    }

    /// Falsifiable connection-reuse proof: TEMP objects are per-connection AND
    /// shadow permanent names, so an empty TEMP VIEW named `headings` is visible
    /// ONLY through the held warm connection. A warm `load_graph_index` that
    /// reuses that connection sees zero headings for a doc that really has one;
    /// a build that cold-opened a fresh connection would see the real heading
    /// and FAIL the assertion — unlike a marker probed via `query_cache`, this
    /// guard cannot pass if the graph build regresses to a fresh open.
    /// (`headings` is the right shadow target: change detection reads only
    /// `documents`, so a no-change refresh never prepares DML against the view;
    /// shadowing `documents` itself would make the refresh see phantom-new files
    /// and error trying to write through the view.)
    /// (Red-proofed: pointing the warm arm back at `cache_cmd::load_graph_index`
    /// makes this test fail.)
    #[test]
    fn warm_load_graph_index_uses_the_held_connection() {
        let (_tmp, root) = make_seeded_vault();
        // A doc with a real heading, indexed on first touch.
        std::fs::write(
            root.join("alpha.md"),
            "---\ntype: note\nstatus: active\n---\n# Alpha\n\nAlpha body\n",
        )
        .unwrap();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");
        let alpha_headings = |index: &crate::core::GraphIndex| -> usize {
            index
                .documents
                .iter()
                .find(|d| d.path == "alpha.md")
                .expect("alpha.md present")
                .headings
                .len()
        };

        // Establish the warm connection.
        ctx.begin_request().expect("begin_request");
        let idx1 = ctx
            .load_graph_index_unscoped()
            .expect("first warm load_graph_index");
        assert_eq!(alpha_headings(&idx1), 1, "alpha's heading is indexed");

        // Shadow the real `headings` table with an empty view on the held
        // connection only. The vault is not touched afterwards, so the next
        // request's incremental refresh detects no changes and writes nothing.
        {
            let cache = ctx
                .query_cache_unscoped()
                .expect("query_cache to plant shadow");
            cache
                .conn()
                .execute_batch("CREATE TEMP VIEW headings AS SELECT * FROM main.headings WHERE 0")
                .unwrap();
        }

        ctx.begin_request().expect("begin_request");
        let idx2 = ctx
            .load_graph_index_unscoped()
            .expect("second warm load_graph_index");
        assert_eq!(
            alpha_headings(&idx2),
            0,
            "the empty shadow must be visible ⇒ the graph build ran on the held \
             warm connection, not a fresh cold open"
        );

        // Control: a genuinely cold open on the same vault state does not see
        // the per-connection shadow.
        let cold_config = load_config(&root.to_path_buf(), None).expect("load_config");
        let cold = crate::cache_cmd::load_graph_index(&root, &cold_config.index_options, false)
            .expect("cold load_graph_index");
        assert_eq!(
            alpha_headings(&cold),
            1,
            "cold open sees the real headings table"
        );
    }

    /// Warm freshness through the graph build: a doc added between calls appears
    /// in the next warm `load_graph_index` (the per-request incremental refresh
    /// runs on the graph-index path, not just on `query_cache` reads).
    #[test]
    fn warm_load_graph_index_refreshes_between_calls() {
        let (tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");

        ctx.begin_request().expect("begin_request");
        let idx1 = ctx
            .load_graph_index_unscoped()
            .expect("first warm load_graph_index");
        assert_eq!(idx1.documents.len(), 3, "three seeded docs");

        std::fs::write(
            tmp.path().join("delta.md"),
            "---\ntype: note\nstatus: active\n---\nDelta body\n",
        )
        .unwrap();

        ctx.begin_request().expect("begin_request");
        let idx2 = ctx
            .load_graph_index_unscoped()
            .expect("second warm load_graph_index");
        assert_eq!(
            idx2.documents.len(),
            4,
            "warm load_graph_index must reflect the doc added between calls"
        );
    }

    /// Verify-once survives a vault-MUTATING request — the property NRN-130
    /// exists for: a warm `load_graph_index` whose per-request refresh actually
    /// WRITES rows (a new doc was added) must still run on the held connection,
    /// not fall back to a fresh open. The two guards above each prove half
    /// (connection reuse without a write; a writing refresh without a
    /// connection probe); this one pins both in a single request.
    ///
    /// The probe is a TEMP VIEW shadowing `headings` that hides alpha's rows on
    /// the held connection only — but unlike the read-only shadow above, a
    /// writing refresh must pass THROUGH it (`drop_document` DELETEs headings
    /// for the added doc, unconditionally), so the view carries pass-through
    /// `INSTEAD OF` triggers forwarding DML to the real table: reads stay
    /// distorted, writes land for real. If the build regressed to a cold open,
    /// the fresh connection would see alpha's real heading and assertion (b)
    /// fails — same falsifiable mechanics as the red-proofed guard above.
    #[test]
    fn warm_load_graph_index_survives_a_writing_refresh() {
        let (tmp, root) = make_seeded_vault();
        // Alpha carries the heading the shadow will hide.
        std::fs::write(
            root.join("alpha.md"),
            "---\ntype: note\nstatus: active\n---\n# Alpha\n\nAlpha body\n",
        )
        .unwrap();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");
        let alpha_headings = |index: &crate::core::GraphIndex| -> usize {
            index
                .documents
                .iter()
                .find(|d| d.path == "alpha.md")
                .expect("alpha.md present")
                .headings
                .len()
        };

        // Establish the warm connection.
        ctx.begin_request().expect("begin_request");
        let idx1 = ctx
            .load_graph_index_unscoped()
            .expect("first warm load_graph_index");
        assert_eq!(idx1.documents.len(), 3, "three seeded docs");
        assert_eq!(alpha_headings(&idx1), 1, "alpha's heading is indexed");

        // Shadow `headings` on the held connection: reads omit alpha's rows,
        // writes forward to the real table so the refresh's DML succeeds.
        {
            let cache = ctx
                .query_cache_unscoped()
                .expect("query_cache to plant shadow");
            cache
                .conn()
                .execute_batch(
                    "CREATE TEMP VIEW headings AS
                       SELECT * FROM main.headings WHERE doc_path <> 'alpha.md';
                     CREATE TEMP TRIGGER headings_shadow_insert
                       INSTEAD OF INSERT ON headings BEGIN
                       INSERT OR IGNORE INTO main.headings
                         (doc_path, level, text, slug,
                          source_span_line, source_span_column, source_span_byte_offset)
                       VALUES (NEW.doc_path, NEW.level, NEW.text, NEW.slug,
                               NEW.source_span_line, NEW.source_span_column,
                               NEW.source_span_byte_offset);
                     END;
                     CREATE TEMP TRIGGER headings_shadow_delete
                       INSTEAD OF DELETE ON headings BEGIN
                       DELETE FROM main.headings
                         WHERE doc_path = OLD.doc_path AND slug = OLD.slug;
                     END;",
                )
                .unwrap();
        }

        // Add a doc WITH a heading, so the next request's incremental refresh
        // writes real rows (documents + headings DML) through the pipeline.
        std::fs::write(
            tmp.path().join("delta.md"),
            "---\ntype: note\nstatus: active\n---\n# Delta\n\nDelta body\n",
        )
        .unwrap();

        ctx.begin_request().expect("begin_request");
        let idx2 = ctx
            .load_graph_index_unscoped()
            .expect("warm load_graph_index across a writing refresh");
        // (a) The refresh happened: the new doc (and its heading) is indexed.
        assert_eq!(idx2.documents.len(), 4, "the writing refresh indexed delta");
        let delta = idx2
            .documents
            .iter()
            .find(|d| d.path == "delta.md")
            .expect("delta.md present");
        assert_eq!(delta.headings.len(), 1, "delta's heading round-tripped");
        // (b) The shadow is still in effect: alpha's heading is hidden, so this
        // build ran on the SAME held connection — no reopen despite the writes.
        assert_eq!(
            alpha_headings(&idx2),
            0,
            "the shadow must still be in effect ⇒ the writing refresh ran on the \
             held warm connection (verify-once survives vault-mutating requests)"
        );

        // Control: a cold open on the same state sees everything for real.
        let cold_config = load_config(&root.to_path_buf(), None).expect("load_config");
        let cold = crate::cache_cmd::load_graph_index(&root, &cold_config.index_options, false)
            .expect("cold load_graph_index");
        assert_eq!(cold.documents.len(), 4);
        assert_eq!(
            alpha_headings(&cold),
            1,
            "cold open sees alpha's real heading (writes landed in the real table)"
        );
    }

    /// A `files.ignore` change is index-relevant, so the NEXT warm
    /// `load_graph_index` must reflect it (the newly-ignored doc absent) — the
    /// full self-heal pipeline (config swap → index-identity mismatch → reopen)
    /// flows through the graph build, not just through `query_cache`.
    #[test]
    fn warm_load_graph_index_honors_files_ignore_change() {
        let (_tmp, root) = make_seeded_vault();
        std::fs::create_dir_all(root.join("Archive").as_std_path()).unwrap();
        std::fs::write(
            root.join("Archive/old.md"),
            "---\ntype: note\n---\nArchived\n",
        )
        .unwrap();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");

        ctx.begin_request().expect("begin_request");
        let before = ctx
            .load_graph_index_unscoped()
            .expect("first warm load_graph_index");
        assert!(
            before
                .documents
                .iter()
                .any(|d| d.path.starts_with("Archive/")),
            "archived doc present before ignore"
        );

        write_config(&root, "files:\n  ignore:\n    - \"Archive/**\"\n");

        ctx.begin_request().expect("begin_request");
        let after = ctx
            .load_graph_index_unscoped()
            .expect("second warm load_graph_index");
        assert!(
            !after
                .documents
                .iter()
                .any(|d| d.path.starts_with("Archive/")),
            "files.ignore change must drop the archived doc from the warm graph index"
        );
    }

    // ---- FIX-1/2/3/6 regression tests (per-request seam) --------------------

    /// FIX-1 (split-brain): within ONE request the config the tool reads and the
    /// cache `query_cache` opens must be the SAME generation. The `begin_request`
    /// seam swaps the config `Arc` BEFORE the tool body reads `config()` or opens
    /// the cache; `ensure_current` then sees the bound generation's index identity
    /// no longer matches and reopens — so an index-relevant change (alias_field)
    /// can't leave one request mixing an old-config graph index with a new-config
    /// cache. Pre-fix, the config swap happened inside `query_cache` (after a tool
    /// already read `config()`), so `config()` read before `query_cache` returned
    /// the stale alias.
    #[test]
    fn warm_begin_request_makes_config_and_cache_same_generation() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");

        // Establish warm state with no alias.
        ctx.begin_request().expect("begin_request");
        {
            let cache = ctx.query_cache_unscoped().expect("first warm query_cache");
            create_marker(&cache);
        }
        assert!(ctx.config().index_options.alias_field.is_none());

        // Index-relevant change.
        write_config(&root, "links:\n  alias_field: aliases\n");

        // ONE request, in a tool's access order: begin_request, THEN read config,
        // THEN open the cache.
        ctx.begin_request()
            .expect("begin_request after config change");
        let config_alias = ctx.config().index_options.alias_field.clone();
        let cache = ctx
            .query_cache_unscoped()
            .expect("query_cache after config change");
        assert_eq!(
            config_alias.as_deref(),
            Some("aliases"),
            "begin_request must swap config before the tool body reads it"
        );
        // The index-relevant drop happened in begin_request, so the cache was
        // reopened this request — proving config and cache are one generation.
        assert!(
            !marker_present(&cache),
            "index-relevant change must reopen the cache in the same request"
        );
    }

    /// FIX-2 (content-hash fingerprint): a same-length config rewrite whose mtime
    /// is restored to the original is invisible to a `(mtime, size)` fingerprint
    /// but must be caught by a content hash. Uses `filetime` to pin mtime.
    #[test]
    fn warm_same_length_config_rewrite_detected_by_content_hash() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");

        // Config A.
        write_config(&root, "links:\n  alias_field: aaaaaa\n");
        ctx.begin_request().expect("begin_request A");
        {
            let _c = ctx.query_cache_unscoped().expect("query_cache A");
        }
        assert_eq!(
            ctx.config().index_options.alias_field.as_deref(),
            Some("aaaaaa")
        );

        let cfg_path = root.join(".norn/config.yaml");
        let orig_mtime = filetime::FileTime::from_last_modification_time(
            &std::fs::metadata(cfg_path.as_std_path()).unwrap(),
        );

        // Config B: SAME LENGTH, then restore the original mtime so a stat-based
        // fingerprint sees no change.
        write_config(&root, "links:\n  alias_field: bbbbbb\n");
        filetime::set_file_mtime(cfg_path.as_std_path(), orig_mtime).unwrap();

        ctx.begin_request().expect("begin_request B");
        {
            let _c = ctx.query_cache_unscoped().expect("query_cache B");
        }
        assert_eq!(
            ctx.config().index_options.alias_field.as_deref(),
            Some("bbbbbb"),
            "a same-length, same-mtime config rewrite must be detected by the content-hash fingerprint"
        );
    }

    /// FIX-3 (corruption self-heal): a corruption-class rusqlite error fed to the
    /// eviction seam invalidates the current generation, so the next request
    /// reopens a NEW generation through the single-flight path.
    #[test]
    fn warm_evicts_state_on_sqlite_corruption_error() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");

        // Hold this request's scope: it binds generation 1, and the corruption
        // seam keys the floor bump off the SCOPE's bound generation (NRN-253).
        let scope = ctx.begin_request().expect("begin_request");
        {
            let cache = ctx.query_cache(&scope).expect("first warm query_cache");
            create_marker(&cache);
        }
        let gen1 = ctx
            .current_generation()
            .expect("warm generation held after first call");
        assert_eq!(gen1.number, 1);

        // Synthesize a SQLITE_CORRUPT failure and feed it to the seam.
        let corrupt = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            Some("database disk image is malformed".to_string()),
        );
        let err = anyhow::Error::new(corrupt).context("tool body failed");
        ctx.note_tool_error(&scope, &err);

        // Next request reopens a NEW generation (marker gone) — the corruption
        // bumped the invalidation floor past generation 1.
        ctx.begin_request().expect("begin_request after eviction");
        let cache = ctx
            .query_cache_unscoped()
            .expect("query_cache after eviction");
        assert!(
            !marker_present(&cache),
            "state must be rebuilt after corruption eviction"
        );
        assert_eq!(doc_count(&cache), 3);
        drop(cache);
        assert_eq!(
            ctx.current_generation()
                .expect("generation after reopen")
                .number,
            2,
            "a corruption-class error must force a new generation on the next request"
        );
    }

    /// FIX-3 (negative): a non-corruption error must NOT evict the warm state.
    #[test]
    fn warm_keeps_state_on_non_corruption_error() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");
        let scope = ctx.begin_request().expect("begin_request");
        {
            let _c = ctx.query_cache(&scope).expect("first warm query_cache");
        }
        assert!(ctx.warm_db_identity().is_some());

        let err = anyhow::anyhow!("some ordinary tool error");
        ctx.note_tool_error(&scope, &err);
        assert!(
            ctx.warm_db_identity().is_some(),
            "a non-corruption error must not evict the warm state"
        );
    }

    /// FIX-3 (bound-generation attribution, NRN-253): a corruption error
    /// attributed via a request's SCOPE to an OLDER generation N must not
    /// invalidate a healthy, newer `current` generation N+1. Models the concurrent-
    /// read-pool scenario the eager `slot.current`-keyed floor bump got wrong: a
    /// request whose scope bound generation 1 fails after generation 2 has already
    /// become current (e.g. another request's ground-shift reopen raced ahead of
    /// it). Keying the floor bump off `scope.bound_generation()` — NOT
    /// `slot.current` — is what keeps generation 2 alive; the next request reuses
    /// it with no spurious reopen.
    #[test]
    fn warm_corruption_error_does_not_invalidate_a_newer_current_generation() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");

        // Request 1's scope binds generation 1 — and is HELD to completion, exactly
        // as an in-flight request would hold it while draining.
        let scope1 = ctx.begin_request().expect("begin_request 1");
        {
            let _c = ctx.query_cache(&scope1).expect("first warm query_cache");
        }
        assert_eq!(ctx.current_generation().expect("generation 1").number, 1);

        // Advance `current` to generation 2 WITHOUT binding it into scope1 — models
        // a DIFFERENT request opening N+1 while request 1's scope still points at N.
        let (_canonical, cache_dir) = cache_dir_for(&root).expect("cache_dir_for");
        std::fs::remove_dir_all(cache_dir.as_std_path()).expect("remove cache dir");
        ctx.force_reopen_without_binding()
            .expect("force reopen to generation 2");
        assert_eq!(ctx.current_generation().expect("generation 2").number, 2);
        assert_eq!(ctx.generation_opens(), 2);

        // Request 1's corruption error surfaces now, attributed via scope1 (which
        // bound generation 1), NOT via whatever is `current` (generation 2).
        let corrupt = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            Some("database disk image is malformed".to_string()),
        );
        ctx.note_tool_error(
            &scope1,
            &anyhow::Error::new(corrupt).context("tool body failed"),
        );

        // Generation 2 must survive: the next request reuses it (no reopen),
        // proving the floor bump targeted generation 1 (scope1's bound generation),
        // not `slot.current`.
        ctx.begin_request().expect("begin_request after error");
        {
            let _c = ctx.query_cache_unscoped().expect("query_cache after error");
        }
        assert_eq!(
            ctx.current_generation()
                .expect("generation still held")
                .number,
            2,
            "a corruption error bound to generation 1 must not evict generation 2"
        );
        assert_eq!(
            ctx.generation_opens(),
            2,
            "no additional reopen — the floor bump must not reach past the errored generation"
        );
    }

    /// NRN-253 per-request note isolation: two request scopes alive on ONE context
    /// simultaneously (the concurrent-read-pool shape, modeled by creating both
    /// scopes directly) each own a PRIVATE note buffer — a note pushed to one lands
    /// only in that scope's drain, never the other's. This is what makes note
    /// forwarding safe once `call_lock` no longer serializes tool bodies: there is
    /// no shared context buffer for concurrent requests to interleave into, so
    /// `run_wrapped`'s drain of one request's scope can never carry another's note.
    #[test]
    fn request_scopes_isolate_operator_notes() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");

        // Two requests' scopes alive at the same time on one context.
        let scope_a = ctx.begin_request().expect("begin_request A");
        let scope_b = ctx.begin_request().expect("begin_request B");

        scope_a.push_operator_note("a-first");
        scope_b.push_operator_note("b-first");
        scope_a.push_operator_note("a-second");

        // Each scope drains ONLY its own notes, in push order — no cross-leak.
        assert_eq!(
            scope_a.take_operator_notes(),
            vec!["a-first".to_string(), "a-second".to_string()],
            "scope A drains only its own notes, in order"
        );
        assert_eq!(
            scope_b.take_operator_notes(),
            vec!["b-first".to_string()],
            "scope B drains only its own note — nothing from A leaked in"
        );
        // A drained scope is empty on a second drain (no residue left behind).
        assert!(
            scope_a.take_operator_notes().is_empty(),
            "a drained scope holds no residual notes"
        );
    }

    /// Panic-recovery (the generational replacement for FIX-6's poison recovery):
    /// a panic while a tool holds the warm cache guard invalidates that
    /// generation via the guard's `Drop` (a floor bump), NOT std-mutex poisoning
    /// (`parking_lot` does not poison). The next request must recover with a
    /// rebuilt generation (marker gone), and the request AFTER must reuse it — the
    /// invalidation is one-shot, not sticky.
    #[test]
    fn warm_recovers_from_panic_holding_the_cache_guard() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = Arc::new(VaultContext::open_warm(&root).expect("open_warm"));

        ctx.begin_request().expect("begin_request");
        {
            let cache = ctx.query_cache_unscoped().expect("first warm query_cache");
            create_marker(&cache);
        }

        // Panic on another thread WHILE holding the cache guard: its `Drop` runs
        // during unwind and bumps the slot's invalidation floor past this
        // generation.
        let ctx2 = Arc::clone(&ctx);
        let handle = std::thread::spawn(move || {
            ctx2.begin_request().expect("begin_request on panic thread");
            let _cache = ctx2
                .query_cache_unscoped()
                .expect("query_cache on panic thread");
            panic!("intentional panic while holding the warm guard");
        });
        assert!(
            handle.join().is_err(),
            "the spawned thread must have panicked while holding the guard"
        );

        // The first post-panic request (request 2) must recover: rebuilt
        // generation, marker gone. Plant a fresh marker here so request 3 below
        // can prove the connection this request opened is REUSED, not re-evicted.
        ctx.begin_request().expect("begin_request after panic");
        {
            let cache = ctx
                .query_cache_unscoped()
                .expect("query_cache must recover from a panic-invalidated generation");
            assert!(
                !marker_present(&cache),
                "warm state must be rebuilt after panic recovery"
            );
            assert_eq!(doc_count(&cache), 3);
            create_marker(&cache);
        }

        // The floor bump must be one-shot, not sticky: request 3 must reuse the
        // SAME connection request 2 rebuilt (marker still present), proving the
        // recovery does not keep re-evicting on every subsequent request.
        ctx.begin_request().expect("begin_request request 3");
        let cache = ctx.query_cache_unscoped().expect("query_cache request 3");
        assert!(
            marker_present(&cache),
            "a second post-panic request must reuse the recovered connection, \
             not evict it again — the invalidation must be one-shot, not sticky"
        );
    }

    /// Coalescing / single-flight for a RE-open (ADR 0013, NRN-251; the reopen
    /// analog of the NRN-55 cold-start regression): after the current generation
    /// is invalidated, N concurrent requests must produce EXACTLY ONE reopen —
    /// every late arrival adopts the generation the first opener produced, so the
    /// counter advances by one, not N.
    ///
    /// The trigger is a corruption invalidation (floor bump): unlike an index-
    /// relevant config change — which itself rebuilds `cache.db` and shifts its
    /// inode, provoking a second, legitimately-distinct ground-shift reopen — a
    /// floor bump reopens the SAME on-disk cache with no rebuild, so a clean
    /// single reopen is the whole story and the count is unambiguous.
    #[test]
    fn warm_concurrent_reopen_coalesces_to_one_open() {
        use std::sync::Barrier;

        let (_tmp, root) = make_seeded_vault();
        let ctx = Arc::new(VaultContext::open_warm(&root).expect("open_warm"));

        // First request opens generation 1; hold its scope to attribute the
        // corruption below to the generation it bound.
        let scope = ctx.begin_request().expect("begin_request");
        {
            let cache = ctx.query_cache(&scope).expect("first warm query_cache");
            assert_eq!(doc_count(&cache), 3);
        }
        assert_eq!(ctx.generation_opens(), 1, "cold start is one open");

        // Invalidate generation 1 for EVERY thread (corruption-class error →
        // floor bump). No config change, no rebuild, no inode shift.
        let corrupt = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            Some("database disk image is malformed".to_string()),
        );
        ctx.note_tool_error(
            &scope,
            &anyhow::Error::new(corrupt).context("tool body failed"),
        );

        const N: usize = 8;
        let barrier = Arc::new(Barrier::new(N));
        let mut handles = Vec::with_capacity(N);
        for _ in 0..N {
            let ctx = Arc::clone(&ctx);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                ctx.begin_request().expect("begin_request");
                let cache = ctx.query_cache_unscoped().expect("query_cache");
                doc_count(&cache)
            }));
        }
        for h in handles {
            assert_eq!(h.join().expect("worker panicked"), 3);
        }

        assert_eq!(
            ctx.generation_opens(),
            2,
            "exactly ONE reopen across N concurrent stale requests (cold start + 1)"
        );
        assert_eq!(
            ctx.current_generation().expect("generation held").number,
            2,
            "the generation counter advanced by exactly one"
        );
    }

    /// Drain-and-drop (ADR 0013, NRN-251): a request in flight on generation N
    /// keeps serving on N while N+1 opens; N's resources (connection + sentinel
    /// fd) release only when its LAST `Arc` drops. Modeled by holding the
    /// generation `Arc` across a reopen: the slot releases its reference (leaving
    /// ours the sole owner) yet N still serves its own snapshot, and the whole
    /// generation is freed exactly when we drop it.
    #[test]
    fn warm_reopen_drains_and_drops_prior_generation() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");

        ctx.begin_request().expect("begin_request");
        {
            let cache = ctx.query_cache_unscoped().expect("first warm query_cache");
            assert_eq!(doc_count(&cache), 3);
        }
        let gen1 = ctx.current_generation().expect("generation 1 held");
        assert_eq!(gen1.number, 1);
        let weak = Arc::downgrade(&gen1);

        // Trigger a reopen via ground-shift: remove the cache dir out from under
        // the live context. gen1's held connection keeps the unlinked db alive
        // (POSIX ghost), so it can still serve its snapshot after this.
        let (_canonical, cache_dir) = cache_dir_for(&root).expect("cache_dir_for");
        std::fs::remove_dir_all(cache_dir.as_std_path()).expect("remove cache dir");

        ctx.begin_request().expect("begin_request");
        {
            let cache = ctx.query_cache_unscoped().expect("second warm query_cache");
            assert_eq!(
                doc_count(&cache),
                3,
                "the reopened generation serves the vault"
            );
        }
        let gen2 = ctx.current_generation().expect("generation 2 held");
        assert_eq!(gen2.number, 2, "reopen advanced the generation number");

        // gen1 drained on N: its still-open connection serves its own snapshot,
        // even though the file was unlinked.
        let gen1_docs: i64 = gen1
            .cache
            .lock()
            .conn()
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .expect("gen1 connection still usable");
        assert_eq!(
            gen1_docs, 3,
            "the drained generation still serves its snapshot"
        );

        // The slot swapped to gen2 and released its gen1 reference — ours is now
        // the sole owner.
        assert_eq!(
            Arc::strong_count(&gen1),
            1,
            "the slot dropped its reference to the prior generation on reopen"
        );
        assert!(
            weak.upgrade().is_some(),
            "generation 1 is still alive while held"
        );

        // Dropping the last `Arc` releases N entirely (connection + sentinel fd
        // close via Drop) — the Weak going dead is the observable Drop-side effect.
        drop(gen1);
        assert!(
            weak.upgrade().is_none(),
            "generation N is fully dropped — connection + sentinel fd released — with its last Arc"
        );
    }

    // ---- Writer-queue generation opens (NRN-252) ----------------------------

    /// Non-`Done` op outcomes map to errors, never a hang or an `unwrap`: a
    /// generation open abandoned by a queue shutdown (`Dropped`) or crashed by a
    /// panic (`Panicked`) surfaces to the `ensure_current` caller as `Err`.
    #[test]
    fn generation_open_non_done_outcomes_map_to_errors() {
        assert!(
            map_open_outcome(Outcome::<Result<Arc<Generation>>>::Dropped).is_err(),
            "a dropped generation-open op must surface as an error"
        );
        assert!(
            map_open_outcome(Outcome::<Result<Arc<Generation>>>::Panicked).is_err(),
            "a panicked generation-open op must surface as an error"
        );
    }

    /// Coalescing THROUGH THE QUEUE, adoption-by-later-queued-op: two open ops are
    /// queued behind a blocker so they run in FIFO on the writer thread. The first
    /// opens generation 2; the SECOND, running after the swap, re-checks `current`,
    /// finds generation 2 fresh, and ADOPTS it — no second open. Complements the
    /// racy 8-thread coalescing test by pinning the exact adoption ordering the
    /// queue's serialization guarantees.
    #[test]
    fn warm_later_queued_open_op_adopts_first_ops_generation() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");

        // Cold start opens generation 1; hold its scope for the corruption below.
        let scope = ctx.begin_request().expect("begin_request");
        {
            let cache = ctx.query_cache(&scope).expect("first warm query_cache");
            assert_eq!(doc_count(&cache), 3);
        }
        assert_eq!(ctx.generation_opens(), 1, "cold start is one open");

        // Invalidate generation 1 (floor bump): same on-disk cache, one clean
        // reopen — so the open count is unambiguous.
        let corrupt = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            Some("database disk image is malformed".to_string()),
        );
        ctx.note_tool_error(
            &scope,
            &anyhow::Error::new(corrupt).context("tool body failed"),
        );

        // Occupy the writer so BOTH open ops queue behind the blocker and then run
        // strictly FIFO — deterministic, no sleeps.
        let (running_tx, running_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let _blocker = ctx.warm_writer_queue().submit_liveness(move || {
            running_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        running_rx.recv().unwrap();

        let h1 = ctx.submit_generation_open();
        let h2 = ctx.submit_generation_open();
        release_tx.send(()).unwrap();

        let g1 = map_open_outcome(h1.wait()).expect("first op opens");
        let g2 = map_open_outcome(h2.wait()).expect("second op adopts");
        assert_eq!(g1.number, 2, "the first queued op opens generation 2");
        assert_eq!(
            g2.number, 2,
            "the later queued op adopts generation 2 rather than opening a third"
        );
        assert_eq!(
            ctx.generation_opens(),
            2,
            "exactly one reopen despite two queued open ops (cold start + 1)"
        );
    }

    /// Shutdown safety: a caller blocked on a generation-open op while the writer
    /// queue shuts down gets an ERROR, never a hang. The op is queued behind a
    /// blocker; dropping the context tears the queue down, so the writer's next
    /// pick observes shutdown and drops the queued op, resolving its handle to
    /// `Dropped` → mapped to `Err`. Deterministic via a shutdown watch (no sleeps).
    #[test]
    fn warm_generation_open_errors_when_queue_shuts_down() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");

        // Establish generation 1 so the slot is warm.
        ctx.begin_request().expect("begin_request");
        {
            let _c = ctx.query_cache_unscoped().expect("first warm query_cache");
        }

        // A watch over the queue's shutdown flag — holds its own `Arc`, so it
        // survives the context drop and lets us order the release after shutdown.
        let watch = ctx.warm_writer_queue().shutdown_watch();

        // Occupy the writer with a blocker so the generation-open op stays QUEUED.
        let (running_tx, running_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let _blocker = ctx.warm_writer_queue().submit_liveness(move || {
            running_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        running_rx.recv().unwrap();

        // Queue the generation-open op behind the blocker.
        let handle = ctx.submit_generation_open();

        // The blocked caller holds ONLY the handle (not the context), so dropping
        // the context can tear the queue down under it.
        let waiter = std::thread::spawn(move || map_open_outcome(handle.wait()));

        // Drop the context from another thread — sets shutdown, then joins on the
        // still-running blocker (mirrors the daemon dropping the vault context).
        let dropper = std::thread::spawn(move || drop(ctx));

        // Once shutdown is committed, release the blocker; the writer's next pick
        // then observes shutdown and DROPS the queued open op. Guaranteed to
        // terminate: the dropper sets the flag.
        while !watch.is_shutting_down() {
            std::thread::yield_now();
        }
        release_tx.send(()).unwrap();

        let result = waiter.join().expect("waiter thread panicked");
        assert!(
            result.is_err(),
            "a generation open abandoned by queue shutdown must be an error, not a hang"
        );
        dropper.join().expect("dropper thread panicked");
    }

    // ---- Request-boundary freshness probe (NRN-253) -------------------------

    /// The observable NRN-253 change: on a vault the probe judges FRESH, a warm
    /// read runs ZERO refresh ops (before NRN-253 every request submitted one).
    /// The first request rebuilds the unbuilt cache via exactly one refresh; a
    /// second request over the unchanged vault probes Fresh and adds none.
    #[test]
    fn warm_fresh_vault_probe_runs_no_refresh() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");

        // First request: the cache is unbuilt, so the probe reports Stale and one
        // refresh (the rebuild) runs.
        ctx.begin_request().expect("begin_request");
        {
            let cache = ctx.query_cache_unscoped().expect("first warm query_cache");
            assert_eq!(doc_count(&cache), 3);
        }
        let gen = ctx.current_generation().expect("generation held");
        let after_build = gen.refresh_exec_count.load(Ordering::Relaxed);
        assert_eq!(
            after_build, 1,
            "the first request rebuilds the unbuilt cache via exactly one refresh"
        );

        // Second request, unchanged vault: probe Fresh ⇒ no further refresh.
        ctx.begin_request().expect("begin_request");
        {
            let cache = ctx.query_cache_unscoped().expect("second warm query_cache");
            assert_eq!(doc_count(&cache), 3);
        }
        assert_eq!(
            gen.refresh_exec_count.load(Ordering::Relaxed),
            after_build,
            "a fresh vault must run NO refresh op — the probe served the read directly"
        );
    }

    /// A stale vault (a doc ADDED between calls) runs EXACTLY ONE refresh and the
    /// request sees the change.
    #[test]
    fn warm_added_file_runs_exactly_one_refresh_and_is_seen() {
        let (tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");

        ctx.begin_request().expect("begin_request");
        {
            ctx.query_cache_unscoped().expect("first warm query_cache");
        }
        let gen = ctx.current_generation().expect("generation held");
        let base = gen.refresh_exec_count.load(Ordering::Relaxed);

        std::fs::write(
            tmp.path().join("delta.md"),
            "---\ntype: note\nstatus: active\n---\nDelta body\n",
        )
        .unwrap();

        ctx.begin_request().expect("begin_request");
        let cache = ctx.query_cache_unscoped().expect("second warm query_cache");
        assert_eq!(doc_count(&cache), 4, "the request sees the added doc");
        assert_eq!(
            gen.refresh_exec_count.load(Ordering::Relaxed),
            base + 1,
            "a stale (added) vault runs exactly one refresh"
        );
    }

    /// The easy miss: a DELETION-only change (no add, no modify) is caught by the
    /// probe (whole-walk file-count shortfall), runs exactly one refresh, and the
    /// request sees the doc purged.
    #[test]
    fn warm_deletion_only_change_detected_by_probe() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");

        ctx.begin_request().expect("begin_request");
        {
            let cache = ctx.query_cache_unscoped().expect("first warm query_cache");
            assert_eq!(doc_count(&cache), 3);
        }
        let gen = ctx.current_generation().expect("generation held");
        let base = gen.refresh_exec_count.load(Ordering::Relaxed);

        std::fs::remove_file(root.join("gamma.md").as_std_path()).unwrap();

        ctx.begin_request().expect("begin_request");
        let cache = ctx.query_cache_unscoped().expect("second warm query_cache");
        assert_eq!(
            doc_count(&cache),
            2,
            "the deletion-only change must be detected and the doc purged"
        );
        assert_eq!(
            gen.refresh_exec_count.load(Ordering::Relaxed),
            base + 1,
            "a deletion-only stale vault runs exactly one refresh"
        );
    }

    /// First-touch: a brand-new warm context whose cache has NEVER been built must
    /// still build — the probe reports Stale(NeverBuilt) so the rebuild runs and
    /// the seeded docs are indexed on the very first request.
    #[test]
    fn warm_first_touch_unbuilt_cache_still_builds() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");

        ctx.begin_request().expect("begin_request");
        let cache = ctx.query_cache_unscoped().expect("first warm query_cache");
        assert_eq!(
            doc_count(&cache),
            3,
            "the first request over an unbuilt cache must build and index the vault"
        );
        let gen = ctx.current_generation().expect("generation held");
        assert_eq!(
            gen.refresh_exec_count.load(Ordering::Relaxed),
            1,
            "the unbuilt cache is rebuilt via exactly one refresh on first touch"
        );
    }

    // ---- Writer-queue freshness refresh (NRN-252) ---------------------------

    /// Separate-connection proof: the freshness refresh runs on the generation's
    /// WRITE connection on the writer thread, so a directly-submitted refresh
    /// completes even while a thread holds the request-facing READ connection's
    /// owned guard. If the refresh still touched the read connection it would
    /// block on the held guard forever (deadlock) and the timeout would fire.
    #[test]
    fn refresh_runs_on_write_connection_not_the_held_read_guard() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = Arc::new(VaultContext::open_warm(&root).expect("open_warm"));

        // Establish generation 1 (its cold-start refresh already ran).
        ctx.begin_request().expect("begin_request");
        {
            let cache = ctx.query_cache_unscoped().expect("first warm query_cache");
            assert_eq!(doc_count(&cache), 3);
        }
        let gen = ctx.current_generation().expect("generation held");

        // Hold the request-facing READ connection's owned guard on THIS thread.
        let read_guard = gen.cache.lock_arc();

        // A refresh submitted now must complete against the WRITE connection,
        // never blocking on the read guard we hold.
        let ctx_refresh = Arc::clone(&ctx);
        let (done_tx, done_rx) = mpsc::channel();
        let refresher = std::thread::spawn(move || {
            let served = matches!(ctx_refresh.test_refresh_current(), RefreshOutcome::Served);
            done_tx.send(served).unwrap();
        });

        let served = done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("refresh must complete without blocking on the held read guard");
        assert!(served, "the refresh should serve cleanly");
        refresher.join().unwrap();

        // The read guard was held for the whole refresh — proof of no contention.
        drop(read_guard);
    }

    /// Coalescing: two requesters that arrive while one refresh op is
    /// queued-but-not-started share ONE execution. The first submits, the second
    /// joins the same ticket, and exactly one `index_incremental` runs.
    #[test]
    fn concurrent_refresh_requesters_coalesce_to_one_execution() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");

        ctx.begin_request().expect("begin_request");
        {
            let _c = ctx.query_cache_unscoped().expect("first warm query_cache");
        }
        let gen = ctx.current_generation().expect("generation held");
        let baseline = gen.refresh_exec_count.load(Ordering::Relaxed);

        // Occupy the writer thread so a submitted refresh op stays QUEUED (not
        // started), leaving a window for a second requester to join it.
        let (running_tx, running_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let _blocker = ctx.warm_writer_queue().submit_liveness(move || {
            running_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        running_rx.recv().unwrap();

        // Two arrivals while the refresh is queued-not-started.
        let a1 = ctx.test_arrive_refresh(&gen);
        let a2 = ctx.test_arrive_refresh(&gen);
        assert!(
            matches!(a1, RefreshArrival::Submitted { .. }),
            "the first arrival submits the refresh op"
        );
        assert!(
            matches!(a2, RefreshArrival::Joined { .. }),
            "the second arrival joins the pending, not-yet-started refresh"
        );
        assert!(
            Arc::ptr_eq(a1.ticket(), a2.ticket()),
            "both requesters share one refresh ticket"
        );

        // Release the writer; the single queued op runs and serves both.
        release_tx.send(()).unwrap();
        assert!(matches!(a1.wait(&gen), RefreshOutcome::Served));
        assert!(matches!(a2.wait(&gen), RefreshOutcome::Served));

        assert_eq!(
            gen.refresh_exec_count.load(Ordering::Relaxed),
            baseline + 1,
            "exactly one refresh execution served both coalesced requesters"
        );
    }

    /// Arrival-correctness: a requester that arrives while a refresh R is already
    /// in flight (started, past its pending slot) must NOT be satisfied by R —
    /// R's scan may predate the requester's world. It must trigger a fresh
    /// refresh and see an edit that landed while R ran.
    #[test]
    fn requester_arriving_during_a_started_refresh_gets_its_own_refresh() {
        let (tmp, root) = make_seeded_vault();
        let ctx = Arc::new(VaultContext::open_warm(&root).expect("open_warm"));

        ctx.begin_request().expect("begin_request");
        {
            let cache = ctx.query_cache_unscoped().expect("first warm query_cache");
            assert_eq!(doc_count(&cache), 3);
        }
        let gen = ctx.current_generation().expect("generation held");
        let baseline = gen.refresh_exec_count.load(Ordering::Relaxed);

        // Arm the one-shot gate: the NEXT refresh op pauses AFTER its start
        // transition (started flag set, pending cleared) and BEFORE its scan.
        let (reached_tx, reached_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        *gen.refresh_gate.lock().unwrap() = Some(TestGate {
            reached: reached_tx,
            release: release_rx,
        });

        // Submit refresh R and drive its blocking wait on a side thread; it will
        // start and then pause at the gate.
        let arrival_r = ctx.test_arrive_refresh(&gen);
        let r_ticket = Arc::clone(arrival_r.ticket());
        let gen_r = Arc::clone(&gen);
        let r_thread =
            std::thread::spawn(move || matches!(arrival_r.wait(&gen_r), RefreshOutcome::Served));

        // R has started: it reached the gate, so it flipped `started` and cleared
        // itself from the pending slot.
        reached_rx.recv().expect("refresh R reaches the gate");
        assert!(r_ticket.is_started(), "R marked itself started");

        // An external edit lands while R is in flight.
        std::fs::write(
            tmp.path().join("delta.md"),
            "---\ntype: note\n---\nDelta body\n",
        )
        .unwrap();

        // A new requester arrives now. R has started (pending cleared), so this
        // requester must submit a FRESH op — never join R.
        let arrival_new = ctx.test_arrive_refresh(&gen);
        assert!(
            matches!(arrival_new, RefreshArrival::Submitted { .. }),
            "a requester arriving after R started must submit a fresh refresh, not join R"
        );
        assert!(
            !Arc::ptr_eq(arrival_new.ticket(), &r_ticket),
            "the new requester's ticket differs from R's"
        );

        // Release R; R finishes, then the new op runs behind it.
        release_tx.send(()).unwrap();
        assert!(r_thread.join().unwrap(), "R served");
        assert!(matches!(arrival_new.wait(&gen), RefreshOutcome::Served));

        // Two distinct executions ran after the baseline (R + the new one): the
        // arriving requester was served by its OWN refresh, not R's.
        assert_eq!(
            gen.refresh_exec_count.load(Ordering::Relaxed),
            baseline + 2,
            "the arriving requester triggered a second, distinct refresh execution"
        );

        // And the edit that landed mid-flight is visible afterward.
        ctx.begin_request().expect("begin_request");
        let cache = ctx
            .query_cache_unscoped()
            .expect("query_cache after refreshes");
        assert_eq!(
            doc_count(&cache),
            4,
            "the requester's own refresh reflects the external edit"
        );
    }

    /// Corruption classification survives the ticket: a corruption-class error
    /// raised by the refresh travels through the coalescing ticket with its
    /// concrete `rusqlite` code intact, so `note_tool_error` still evicts the
    /// generation. Mirrors `warm_evicts_state_on_sqlite_corruption_error`, now
    /// routed through the queue-refresh path.
    #[test]
    fn refresh_corruption_error_survives_ticket_and_evicts_generation() {
        let (tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");

        ctx.begin_request().expect("begin_request");
        {
            let cache = ctx.query_cache_unscoped().expect("first warm query_cache");
            create_marker(&cache);
        }
        let gen1 = ctx.current_generation().expect("generation 1 held");
        assert_eq!(gen1.number, 1);

        // Inject a SQLITE_CORRUPT failure into gen1's NEXT refresh.
        let corrupt = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            Some("database disk image is malformed".to_string()),
        );
        *gen1.inject_refresh_error.lock().unwrap() = Some(CacheError::Sqlite(corrupt));

        // Dirty the vault so the NRN-253 probe reports Stale and routes the request
        // through the refresh — where the injected corruption fires. (Before
        // NRN-253 the refresh ran unconditionally; now an unchanged vault would
        // probe Fresh and never reach the refresh, so the failure must be staged
        // behind a real change.)
        std::fs::write(
            tmp.path().join("delta.md"),
            "---\ntype: note\n---\nDelta body\n",
        )
        .unwrap();

        // The next request's refresh fails; the concrete error must propagate out
        // of query_cache still classifiable as corruption. `query_cache` binds the
        // generation into the scope BEFORE the refresh runs, so the scope carries
        // the bound generation even though the call returns Err.
        let scope = ctx.begin_request().expect("begin_request");
        let err = ctx
            .query_cache(&scope)
            .expect_err("a corruption refresh error must propagate out of query_cache");
        assert!(
            is_sqlite_corruption(&err),
            "the concrete CacheError survived the ticket and is still classifiable"
        );

        // Feed it to the eviction seam exactly as `run_wrapped` does.
        ctx.note_tool_error(&scope, &err);

        // The next request reopens a NEW generation (marker gone, number bumped).
        ctx.begin_request().expect("begin_request after eviction");
        let cache = ctx
            .query_cache_unscoped()
            .expect("query_cache after eviction");
        assert!(
            !marker_present(&cache),
            "state must be rebuilt after the ticket-routed corruption eviction"
        );
        // 4, not 3: the reopened generation rebuilds against the live vault, which
        // now carries the `delta.md` staged above to force the stale-probe refresh.
        assert_eq!(doc_count(&cache), 4);
        drop(cache);
        assert_eq!(
            ctx.current_generation()
                .expect("generation after reopen")
                .number,
            2,
            "a corruption refresh error routed through the ticket must force a new generation"
        );
    }

    /// Set up two coalesced refresh requesters onto ONE not-yet-started refresh
    /// op whose execution returns `inject`, returning both un-waited arrivals.
    /// The writer is occupied by a blocker until `release` is signalled so both
    /// arrivals land before the op starts and share one ticket.
    #[cfg(test)]
    fn two_coalesced_arrivals_over_injected_refresh(
        ctx: &VaultContext,
        gen: &Arc<Generation>,
        inject: CacheError,
    ) -> (RefreshArrival, RefreshArrival, mpsc::Sender<()>) {
        *gen.inject_refresh_error.lock().unwrap() = Some(inject);

        let (running_tx, running_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        // Dropping the blocker's Handle does not stop the op — the writer thread
        // owns the running closure — so the writer stays occupied until release,
        // holding the coalescing window open. The op's result channel simply
        // disconnects when it finishes, which the test never reads.
        let _ = ctx.warm_writer_queue().submit_liveness(move || {
            running_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        running_rx.recv().unwrap();

        let a1 = ctx.test_arrive_refresh(gen);
        let a2 = ctx.test_arrive_refresh(gen);
        assert!(
            matches!(a1, RefreshArrival::Submitted { .. }),
            "the first arrival submits the refresh op"
        );
        assert!(
            matches!(a2, RefreshArrival::Joined { .. }),
            "the second arrival joins the pending, not-yet-started op"
        );
        assert!(
            Arc::ptr_eq(a1.ticket(), a2.ticket()),
            "both requesters share one refresh ticket"
        );
        (a1, a2, release_tx)
    }

    /// FIX-1 (outcome fan-out): two requesters coalesced onto ONE refresh that
    /// FAILS must BOTH observe a failure — no coalesced waiter may read a failed
    /// refresh as `Served`. Pre-fix, the first waiter took the `Result` and every
    /// later coalesced waiter hit `None => Served`, silently masking the failure
    /// (corruption-class included) for all but the first.
    #[test]
    fn coalesced_waiters_on_a_failed_refresh_all_observe_failure() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");
        ctx.begin_request().expect("begin_request");
        {
            let _c = ctx.query_cache_unscoped().expect("first warm query_cache");
        }
        let gen = ctx.current_generation().expect("generation held");

        let corrupt = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            Some("database disk image is malformed".to_string()),
        );
        let (a1, a2, release) =
            two_coalesced_arrivals_over_injected_refresh(&ctx, &gen, CacheError::Sqlite(corrupt));

        release.send(()).unwrap();

        let o1 = a1.wait(&gen);
        let o2 = a2.wait(&gen);
        assert!(
            matches!(o1, RefreshOutcome::Failed(_)),
            "the submitting waiter must observe the failure, got a non-Failed outcome"
        );
        assert!(
            matches!(o2, RefreshOutcome::Failed(_)),
            "the coalesced waiter must observe a failure, NOT Served (the take()-masking bug)"
        );
    }

    /// FIX-1 (outcome fan-out): two requesters coalesced onto ONE refresh that
    /// times out on the write lock must BOTH observe `LockContention` — so each
    /// emits its own NRN-215 both-surfaces note — never `Served`.
    #[test]
    fn coalesced_waiters_on_a_locktimeout_refresh_all_observe_contention() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");
        ctx.begin_request().expect("begin_request");
        {
            let _c = ctx.query_cache_unscoped().expect("first warm query_cache");
        }
        let gen = ctx.current_generation().expect("generation held");

        let (a1, a2, release) =
            two_coalesced_arrivals_over_injected_refresh(&ctx, &gen, CacheError::LockTimeout);

        release.send(()).unwrap();

        let o1 = a1.wait(&gen);
        let o2 = a2.wait(&gen);
        assert!(
            matches!(o1, RefreshOutcome::LockContention),
            "the submitting waiter must observe LockContention"
        );
        assert!(
            matches!(o2, RefreshOutcome::LockContention),
            "the coalesced waiter must observe LockContention, NOT Served"
        );
    }

    // ---- Post-apply increment commit (NRN-252 / NRN-158) --------------------

    fn doc_present(cache: &Cache, path: &str) -> bool {
        let n: i64 = cache
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM documents WHERE path = ?",
                [path],
                |r| r.get(0),
            )
            .unwrap();
        n == 1
    }

    fn body_of(cache: &Cache, path: &str) -> Option<String> {
        cache
            .conn()
            .query_row(
                "SELECT body_text FROM documents WHERE path = ?",
                [path],
                |r| r.get(0),
            )
            .ok()
    }

    /// THE NRN-158 acceptance test: a warm mutation via the `set` tool commits its
    /// own cache increment, so the FOLLOWING read's refresh detects zero changes
    /// and runs no whole-vault rebuild.
    #[test]
    fn warm_mutation_commits_increment_so_next_read_sees_no_changes() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");

        // Open the generation and build the cache (3 seeded docs).
        ctx.begin_request().expect("begin_request");
        {
            let cache = ctx.query_cache_unscoped().expect("query_cache");
            assert_eq!(doc_count(&cache), 3);
        }

        // Warm mutation (confirm): applies on disk AND commits its increment.
        let scope = ctx.begin_request().expect("begin_request");
        let report = crate::mcp::tools::set::handle(
            &ctx,
            &scope,
            crate::mcp::tools::set::SetParams {
                target: "beta".into(),
                field_json: vec![r#"status="active""#.into()],
                confirm: true,
                ..Default::default()
            },
        )
        .expect("set confirm should apply");
        assert!(report.applied, "the set must apply");

        // The point of NRN-158: the next read's detect scan finds NOTHING.
        assert_eq!(
            ctx.detect_change_count(),
            0,
            "the committed increment must leave detect with no work"
        );

        // And an actual freshness refresh produces an EMPTY report — proof it did
        // not re-run the whole-vault rebuild.
        ctx.begin_request().expect("begin_request");
        assert!(matches!(ctx.test_refresh_current(), RefreshOutcome::Served));
        let refresh = ctx.last_refresh_report().expect("a refresh ran");
        assert_eq!(
            (refresh.doc_count, refresh.file_count, refresh.link_count),
            (0, 0, 0),
            "the post-increment refresh must detect zero changes (no rebuild)"
        );
    }

    /// File coherence: with a 0ms budget forcing one file per chunk, a file
    /// present mid-stream carries its new content in full, and a not-yet-committed
    /// file is entirely absent — never a mix.
    #[test]
    fn increment_commits_files_coherently_across_chunks() {
        // 0ms budget ⇒ one whole file per chunk. Process-global but inert: it only
        // makes other increments chunk more finely, never changes correctness.
        std::env::set_var("NORN_CACHE_INCREMENT_BUDGET_MS", "0");

        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");
        ctx.begin_request().expect("begin_request");
        {
            let cache = ctx.query_cache_unscoped().expect("query_cache");
            assert_eq!(doc_count(&cache), 3);
        }
        let generation = ctx.current_generation().expect("a current generation");

        // Modify alpha's body; create delta. Sorted order commits alpha first.
        std::fs::write(
            root.join("alpha.md"),
            "---\ntype: note\nstatus: active\n---\nAlpha REVISED body\n",
        )
        .unwrap();
        std::fs::write(root.join("delta.md"), "---\ntype: note\n---\nDelta body\n").unwrap();
        let changed: Vec<Utf8PathBuf> = vec!["alpha.md".into(), "delta.md".into()];

        let (reached, release) = ctx.install_increment_gate(&generation);
        let handle = ctx.test_submit_increment_commit(&generation, &changed);

        // Boundary 1: alpha's chunk committed; delta's has NOT started.
        reached.recv().expect("boundary 1");
        {
            let cache = generation.cache.lock();
            assert!(
                body_of(&cache, "alpha.md")
                    .as_deref()
                    .is_some_and(|b| b.contains("REVISED")),
                "alpha must be fully swapped to its new body at boundary 1"
            );
            assert!(
                !doc_present(&cache, "delta.md"),
                "delta must be ABSENT until its own chunk commits (file coherence)"
            );
        }
        release.send(()).unwrap();

        // Boundary 2: delta's chunk committed.
        reached.recv().expect("boundary 2");
        {
            let cache = generation.cache.lock();
            assert!(
                doc_present(&cache, "delta.md"),
                "delta present after its own chunk commits"
            );
        }
        release.send(()).unwrap();

        assert!(
            matches!(handle.wait(), Outcome::Done(Ok(()))),
            "the increment commit must complete cleanly"
        );
        assert_eq!(
            ctx.detect_change_count(),
            0,
            "a completed increment leaves detect empty"
        );
    }

    /// Preemption: a liveness op submitted while a multi-chunk increment is in
    /// flight runs BETWEEN two increment chunks, not after all of them.
    #[test]
    fn liveness_op_preempts_increment_between_chunks() {
        std::env::set_var("NORN_CACHE_INCREMENT_BUDGET_MS", "0");

        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");
        ctx.begin_request().expect("begin_request");
        {
            let cache = ctx.query_cache_unscoped().expect("query_cache");
            assert_eq!(doc_count(&cache), 3);
        }
        let generation = ctx.current_generation().expect("a current generation");

        std::fs::write(root.join("delta.md"), "---\ntype: note\n---\nDelta\n").unwrap();
        std::fs::write(root.join("epsilon.md"), "---\ntype: note\n---\nEpsilon\n").unwrap();
        let changed: Vec<Utf8PathBuf> = vec!["delta.md".into(), "epsilon.md".into()];

        let (reached, release) = ctx.install_increment_gate(&generation);
        let inc = ctx.test_submit_increment_commit(&generation, &changed);

        // First chunk committed; the bulk op is parked at boundary 1.
        reached.recv().expect("boundary 1");

        // Submit a liveness op while the increment is mid-flight.
        let live = ctx.warm_writer_queue().submit_liveness(|| 42u8);

        // Release the boundary: the queue must drain the liveness op BEFORE the
        // next increment chunk.
        release.send(()).unwrap();
        assert_eq!(
            live.wait(),
            Outcome::Done(42),
            "the liveness op must run once the boundary clears"
        );

        // A LATER increment boundary still arrives — proof the liveness op ran
        // BETWEEN two chunks, not after the whole op.
        reached.recv().expect("boundary 2 after the liveness op");
        release.send(()).unwrap();

        assert!(
            matches!(inc.wait(), Outcome::Done(Ok(()))),
            "the increment still completes after being preempted"
        );
    }

    /// Drop-on-generation-death: turning the generation stale mid-commit drops the
    /// remaining chunks WITHOUT running them, and the tool seam still returns
    /// (never fails) with the deferred operator note.
    #[test]
    fn increment_dropped_on_generation_death_tool_still_succeeds() {
        std::env::set_var("NORN_CACHE_INCREMENT_BUDGET_MS", "0");

        let (_tmp, root) = make_seeded_vault();
        let ctx = Arc::new(VaultContext::open_warm(&root).expect("open_warm"));
        let scope = ctx.begin_request().expect("begin_request");
        {
            let cache = ctx.query_cache(&scope).expect("query_cache");
            assert_eq!(doc_count(&cache), 3);
        }
        let generation = ctx.current_generation().expect("a current generation");

        std::fs::write(root.join("delta.md"), "---\ntype: note\n---\nDelta\n").unwrap();
        std::fs::write(root.join("epsilon.md"), "---\ntype: note\n---\nEpsilon\n").unwrap();
        let changed: Vec<Utf8PathBuf> = vec!["delta.md".into(), "epsilon.md".into()];

        let (reached, release) = ctx.install_increment_gate(&generation);

        // Drive the REAL tool seam (which awaits) on a worker thread so this thread
        // can evict the generation mid-commit. The request's scope moves onto the
        // worker (the increment op pushes its degrade note into it — NRN-253); the
        // worker returns the drained notes back for assertion.
        let ctx_worker = Arc::clone(&ctx);
        let changed_worker = changed.clone();
        let worker = std::thread::spawn(move || {
            ctx_worker.commit_apply_increments(&scope, &changed_worker);
            scope.take_operator_notes()
        });

        // First chunk (delta) committed; op parked at boundary 1.
        reached.recv().expect("boundary 1");
        // Evict the generation: bump the invalidation floor past it.
        ctx.test_invalidate_current_generation();
        // Release: the still_valid guard now reads false, so the op is DROPPED
        // before the next chunk (epsilon) runs.
        release.send(()).unwrap();

        // The tool seam returns normally (never fails) despite the drop, and hands
        // back its scope's drained notes.
        let notes = worker
            .join()
            .expect("commit_apply_increments must not panic");

        // The remaining chunk never ran — only delta committed.
        {
            let cache = generation.cache.lock();
            assert!(
                doc_present(&cache, "delta.md"),
                "delta (committed before the death) is present"
            );
            assert!(
                !doc_present(&cache, "epsilon.md"),
                "epsilon's chunk must NOT run after the generation died"
            );
        }

        // The degrade left the both-surfaces operator note (drained above).
        assert!(
            notes.iter().any(|n| n.contains("abandoned")),
            "a dropped increment must leave the deferred operator note, got {notes:?}"
        );
    }
}
