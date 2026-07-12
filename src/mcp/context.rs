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
//! mutates. (The DB *content* still changes — a generation's connection runs the
//! per-request incremental refresh and mutation tools write through it — but the
//! *binding* is fixed.) Every request binds the current `Arc<Generation>` (and
//! the current config `Arc`) at its boundary and holds both to completion, so no
//! request ever observes a swap mid-flight.
//!
//! Every evict/re-open trigger — cold start / first touch, ground-shift
//! (out-of-band `cache clear` / `prune` / `rm`), cache-identity change /
//! corruption, and an index-relevant config change — routes through the ONE
//! single-flight path [`VaultContext::ensure_current`], which opens generation
//! N+1 and swaps the slot's current pointer. Concurrent stale observers of
//! generation N coalesce to exactly one open (the open guard + a re-check of the
//! current pointer under it). In-flight requests keep serving on N; N drops with
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
//!    now-different index identity, single-flight open generation N+1. See the
//!    sentinel-discipline notes on `open_generation` for the ordering that keeps
//!    identity honest. This is the ONLY place `integrity_check` is paid in warm
//!    mode.
//! 4. **Freshness.** Run the same lock-timeout-tolerant `index_incremental`
//!    refresh cold mode gets, so vault edits between calls are reflected.
//!
//! Warm mode is only ever constructed with the default config location; the
//! daemon wire never carries a custom `--config` path, so `open_warm` takes only
//! `cwd` and hard-codes `config_path = None`.

use std::fs::File;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};
use parking_lot::{ArcMutexGuard, Mutex as CacheMutex};

use crate::cache::{cache_dir_for, Cache, CacheError, ChangeDetectOptions};
use crate::cache_cmd::open_for_query;
use crate::config_loader::{load_config, LoadedConfig};

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
}

/// An immutable warm-cache generation (ADR 0013). Once opened, the
/// `(cache identity, index identity, sentinel)` binding never mutates; a request
/// binds the current `Arc<Generation>` at its boundary and holds it to
/// completion. The DB *content* still changes — the per-request incremental
/// refresh and mutation tools write through the connection — which is why the
/// `Cache` sits behind an owned-lockable mutex; everything else is fixed for the
/// generation's life. Dropping the last `Arc` closes the connection and the
/// sentinel fd via `Drop`, so an evicted generation releases exactly when its
/// last in-flight request finishes draining on it.
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
    /// The held-open connection. `Arc<Mutex>` so a request can take an OWNED lock
    /// (`lock_arc`) held across the whole sync tool body, outliving a concurrent
    /// generation swap of the slot's current pointer. `parking_lot` (not `std`):
    /// its `ArcMutexGuard` is a safe owned guard, and a panic while a tool holds
    /// it is handled precisely by the handle's `Drop` (invalidation floor bump),
    /// not by std-mutex poisoning.
    cache: Arc<CacheMutex<Cache>>,
}

/// Warm-mode per-vault slot. The small fields are guarded by `std::sync::Mutex`
/// (NOT tokio), locked only briefly and NEVER across an `.await` (tool bodies are
/// sync); the cache connection's own lock lives inside [`Generation`].
struct WarmSlot {
    /// The current generation, `None` only before the first open. Never nulled in
    /// place afterward: a reopen swaps this pointer to N+1 and the old N drops
    /// with its last in-flight `Arc`.
    current: Mutex<Option<Arc<Generation>>>,
    /// Fingerprint of the config file; independent of any generation, so it
    /// survives reopens.
    config_fp: Mutex<ConfigFingerprint>,
    /// Single-flight generation-open guard. Held only for the open critical
    /// section (probe re-check → open → swap), so concurrent stale observers of
    /// generation N coalesce to exactly ONE open of N+1. Also owns the monotonic
    /// generation counter.
    open: Mutex<u64>,
    /// Monotonic invalidation floor: a generation whose `number` is BELOW this is
    /// stale and must be reopened. Bumped by corruption ([`note_tool_error`]) and
    /// by a panic unwinding through a live cache guard — the two triggers with no
    /// filesystem/config signal for `ensure_current` to observe. An atomic so the
    /// per-request freshness probe reads it without taking the open lock; shared
    /// with each handed-out cache guard so its panic-`Drop` can bump it.
    floor: Arc<AtomicU64>,
    /// Total generation opens performed (cold start + every reopen). Test-only
    /// coalescing/single-flight assertion counter; incremented on every open.
    open_count: AtomicU64,
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
    /// Operator notes generated while serving the CURRENT request — e.g. the
    /// lock-contention note `query_cache_warm` produces when the implicit
    /// refresh times out on the write lock. The tool funnel
    /// ([`McpServer::run_wrapped`](crate::mcp::server::McpServer)) clears this at
    /// the per-request seam ([`begin_request`](Self::begin_request)) and drains
    /// it into the tool's `structuredContent.operator_notes` after the body runs,
    /// both under the process-wide `call_lock` — so notes cannot leak across the
    /// serialized requests of concurrent connections (NRN-215). On the direct
    /// (non-daemon) path these notes still go straight to the CLI's stderr; this
    /// buffer only exists so the DAEMON path can forward them to the routed CLI
    /// instead of losing them on the daemon's own stderr.
    operator_notes: Mutex<Vec<String>>,
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
            operator_notes: Mutex::new(Vec::new()),
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
                current: Mutex::new(None),
                config_fp: Mutex::new(config_fp),
                // Generation numbers start at 1, so the counter's next value is 1.
                open: Mutex::new(1),
                floor: Arc::new(AtomicU64::new(0)),
                open_count: AtomicU64::new(0),
            }),
            operator_notes: Mutex::new(Vec::new()),
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
    pub(crate) fn begin_request(&self) -> Result<()> {
        // Clear any operator notes left from a prior request FIRST (both modes),
        // so this request starts with an empty buffer. Combined with the drain in
        // `run_wrapped` — both under the process-wide `call_lock` — this bounds
        // every note to the single request that produced it (NRN-215).
        self.operator_notes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();

        let Mode::Warm(slot) = &self.mode else {
            return Ok(());
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
        Ok(())
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
    pub(crate) fn note_tool_error(&self, err: &anyhow::Error) {
        let Mode::Warm(slot) = &self.mode else {
            return;
        };
        if is_sqlite_corruption(err) {
            if let Some(gen) = slot
                .current
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
            {
                slot.floor.fetch_max(gen.number + 1, Ordering::AcqRel);
            }
        }
    }

    /// Record an operator note for the current request. Called from the read
    /// pipeline (e.g. [`query_cache_warm`](Self::query_cache_warm) on a write-lock
    /// timeout); drained by `run_wrapped` into the tool envelope's
    /// `operator_notes` (NRN-215). A poisoned lock is recovered in place — a lost
    /// note is a strictly better failure mode than panicking the request.
    pub(crate) fn push_operator_note(&self, note: impl Into<String>) {
        self.operator_notes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(note.into());
    }

    /// Drain and return the operator notes accumulated for the current request.
    /// The tool funnel calls this under `call_lock` immediately after the tool
    /// body, so the returned notes belong to exactly this request.
    pub(crate) fn take_operator_notes(&self) -> Vec<String> {
        std::mem::take(
            &mut *self
                .operator_notes
                .lock()
                .unwrap_or_else(|p| p.into_inner()),
        )
    }

    /// The current config. Locks briefly, clones the `Arc`, and releases — so a
    /// warm config hot-swap can proceed independently of callers still reading
    /// through an earlier `Arc`. A poisoned lock is recovered in place (the value
    /// is an immutable `Arc` snapshot, so there is nothing to evict) rather than
    /// panicking on every subsequent request.
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
    pub(crate) fn query_cache(&self) -> Result<CacheHandle> {
        match &self.mode {
            Mode::Cold => {
                let config = self.config();
                let cache = open_for_query(&self.vault_root, &config.index_options, false)?;
                Ok(CacheHandle::Owned(cache))
            }
            Mode::Warm(slot) => self.query_cache_warm(slot),
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
    pub(crate) fn load_graph_index(&self) -> Result<crate::core::GraphIndex> {
        let cache = self.query_cache()?;
        Ok(cache.load_graph_index()?)
    }

    /// The warm per-request pipeline (steps 2–4). See the module-level docs for
    /// the ordered rationale of each step.
    fn query_cache_warm(&self, slot: &WarmSlot) -> Result<CacheHandle> {
        // Steps 0–1 (root-liveness + config-freshness) already ran in
        // `begin_request` at the per-request seam, so config is stable here for
        // the whole request. Steps 2–3 (ground-shift + reopen-if-stale) are the
        // single-flight `ensure_current`, which returns the generation this
        // request binds for its whole body.
        let generation = self.ensure_current(slot)?;

        // Take an OWNED lock on the bound generation's connection, held across the
        // whole (sync) tool body. It outlives a concurrent generation swap of the
        // slot pointer because the guard owns its own `Arc<Mutex<Cache>>` and the
        // handle also holds the `Arc<Generation>` (keeping the sentinel alive).
        let mut guard = generation.cache.lock_arc();

        // Step 4 — freshness. Same lock-timeout tolerance as `open_for_query`.
        match guard.index_incremental(&self.vault_root, &ChangeDetectOptions::default()) {
            Ok(_) => {}
            Err(CacheError::LockTimeout) => {
                // BOTH surfaces (NRN-215): the daemon's own stderr is its
                // operational log — an operator tailing `norn serve` (or a
                // log pipeline) keeps the contention signal, alongside the
                // served markers — AND the per-request note buffer carries
                // it to the caller: `run_wrapped` forwards it in the tool
                // envelope and the routed CLI re-emits it on ITS stderr,
                // byte-identical to a direct run.
                eprintln!("{}", crate::cache::LOCK_CONTENTION_NOTE);
                self.push_operator_note(crate::cache::LOCK_CONTENTION_NOTE);
            }
            Err(error) => return Err(error.into()),
        }

        Ok(CacheHandle::Warm(WarmGuard {
            number: generation.number,
            floor: Arc::clone(&slot.floor),
            _generation: generation,
            guard,
        }))
    }

    /// The ONE single-flight generation-open path (steps 2–3). Returns the
    /// current generation, opening N+1 when the current one is absent, ground-
    /// shifted, invalidated (below the floor), or opened under a now-different
    /// index identity. Concurrent stale observers coalesce to exactly one open:
    /// the `open` guard serializes them and a re-check of the current pointer
    /// under it lets a late arrival adopt the generation an earlier one just
    /// produced.
    fn ensure_current(&self, slot: &WarmSlot) -> Result<Arc<Generation>> {
        let config = self.config();

        // Fast path: probe the current generation off the open lock.
        {
            let snapshot = slot
                .current
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            if let Some(generation) = &snapshot {
                if self.generation_is_fresh(generation, slot, &config) {
                    return Ok(Arc::clone(generation));
                }
            }
        }

        // Stale or cold → single-flight open. Holding `open` coalesces concurrent
        // openers; the FIRST performs the open, the rest re-check below and adopt.
        let mut counter = slot.open.lock().unwrap_or_else(|p| p.into_inner());
        {
            let snapshot = slot
                .current
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            if let Some(generation) = &snapshot {
                if self.generation_is_fresh(generation, slot, &config) {
                    return Ok(Arc::clone(generation));
                }
            }
        }

        // Step 3 — open generation N+1. This is the ONLY place integrity_check is
        // paid in warm mode; a stable connection is reused across requests.
        let number = *counter;
        *counter += 1;
        let generation = Arc::new(open_generation(&self.vault_root, &config, number)?);
        slot.open_count.fetch_add(1, Ordering::Relaxed);
        *slot.current.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::clone(&generation));
        Ok(generation)
    }

    /// Is `generation` still fresh for the current world? False when it has been
    /// invalidated (corruption / panic bumped the floor past it), when its
    /// `cache.db` identity no longer matches the live file (ground-shift), or when
    /// the live config's index identity differs from the one it was opened under
    /// (an index-relevant config change).
    fn generation_is_fresh(
        &self,
        generation: &Generation,
        slot: &WarmSlot,
        config: &LoadedConfig,
    ) -> bool {
        generation.number >= slot.floor.load(Ordering::Acquire)
            && current_db_identity(&self.vault_root) == Some(generation.db_identity)
            && generation.index_identity == IndexIdentity::from_config(config)
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
            Mode::Warm(slot) => slot.open_count.load(Ordering::Relaxed),
            Mode::Cold => 0,
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

/// Open a fresh [`Generation`]: the held-open cache connection plus the `(dev,
/// ino)` identity and index identity we will verify it against on later
/// requests. Called only from [`VaultContext::ensure_current`] under the slot's
/// single-flight `open` guard.
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
    Ok(Generation {
        number,
        db_identity,
        index_identity: IndexIdentity::from_config(config),
        _sentinel: sentinel,
        cache: Arc::new(CacheMutex::new(cache)),
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

        let cache = ctx.query_cache().expect("query_cache should return Ok");

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
                .query_cache()
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
                .query_cache()
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
            let cache = ctx.query_cache().expect("first warm query_cache");
            create_marker(&cache);
        }
        let id1 = ctx.warm_db_identity();
        assert!(id1.is_some(), "warm state should be held after first call");

        {
            ctx.begin_request().expect("begin_request");
            let cache = ctx.query_cache().expect("second warm query_cache");
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
            let cache = ctx.query_cache().expect("first warm query_cache");
            assert_eq!(doc_count(&cache), 3, "initial count should be 3");
        }

        std::fs::write(
            tmp.path().join("delta.md"),
            "---\ntype: note\nstatus: active\n---\nDelta body\n",
        )
        .unwrap();

        {
            ctx.begin_request().expect("begin_request");
            let cache = ctx.query_cache().expect("second warm query_cache");
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
            let cache = ctx.query_cache().expect("first warm query_cache");
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
                .query_cache()
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
            let cache = ctx.query_cache().expect("first warm query_cache");
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
            let cache = ctx.query_cache().expect("first warm query_cache");
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
                .query_cache()
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
            let cache = ctx.query_cache().expect("first warm query_cache");
            create_marker(&cache);
        }

        write_config(&root, "validate:\n  ignore:\n    - \"logs/**\"\n");

        {
            ctx.begin_request().expect("begin_request");
            let cache = ctx
                .query_cache()
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
            let cache = ctx.query_cache().expect("first warm query_cache");
            create_marker(&cache);
        }
        assert!(ctx.config().index_options.alias_field.is_none());

        // Index-relevant: alias_field feeds Cache::open_with_index.
        write_config(&root, "links:\n  alias_field: aliases\n");

        {
            ctx.begin_request().expect("begin_request");
            let cache = ctx
                .query_cache()
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
            let cache = ctx.query_cache().expect("first warm query_cache");
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
                .query_cache()
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
        let warm = ctx.load_graph_index().expect("warm load_graph_index");

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
        let idx1 = ctx.load_graph_index().expect("first warm load_graph_index");
        assert_eq!(alpha_headings(&idx1), 1, "alpha's heading is indexed");

        // Shadow the real `headings` table with an empty view on the held
        // connection only. The vault is not touched afterwards, so the next
        // request's incremental refresh detects no changes and writes nothing.
        {
            let cache = ctx.query_cache().expect("query_cache to plant shadow");
            cache
                .conn()
                .execute_batch("CREATE TEMP VIEW headings AS SELECT * FROM main.headings WHERE 0")
                .unwrap();
        }

        ctx.begin_request().expect("begin_request");
        let idx2 = ctx
            .load_graph_index()
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
        let idx1 = ctx.load_graph_index().expect("first warm load_graph_index");
        assert_eq!(idx1.documents.len(), 3, "three seeded docs");

        std::fs::write(
            tmp.path().join("delta.md"),
            "---\ntype: note\nstatus: active\n---\nDelta body\n",
        )
        .unwrap();

        ctx.begin_request().expect("begin_request");
        let idx2 = ctx
            .load_graph_index()
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
        let idx1 = ctx.load_graph_index().expect("first warm load_graph_index");
        assert_eq!(idx1.documents.len(), 3, "three seeded docs");
        assert_eq!(alpha_headings(&idx1), 1, "alpha's heading is indexed");

        // Shadow `headings` on the held connection: reads omit alpha's rows,
        // writes forward to the real table so the refresh's DML succeeds.
        {
            let cache = ctx.query_cache().expect("query_cache to plant shadow");
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
            .load_graph_index()
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
        let before = ctx.load_graph_index().expect("first warm load_graph_index");
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
            .load_graph_index()
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
            let cache = ctx.query_cache().expect("first warm query_cache");
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
        let cache = ctx.query_cache().expect("query_cache after config change");
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
            let _c = ctx.query_cache().expect("query_cache A");
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
            let _c = ctx.query_cache().expect("query_cache B");
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

        ctx.begin_request().expect("begin_request");
        {
            let cache = ctx.query_cache().expect("first warm query_cache");
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
        ctx.note_tool_error(&err);

        // Next request reopens a NEW generation (marker gone) — the corruption
        // bumped the invalidation floor past generation 1.
        ctx.begin_request().expect("begin_request after eviction");
        let cache = ctx.query_cache().expect("query_cache after eviction");
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
        ctx.begin_request().expect("begin_request");
        {
            let _c = ctx.query_cache().expect("first warm query_cache");
        }
        assert!(ctx.warm_db_identity().is_some());

        let err = anyhow::anyhow!("some ordinary tool error");
        ctx.note_tool_error(&err);
        assert!(
            ctx.warm_db_identity().is_some(),
            "a non-corruption error must not evict the warm state"
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
            let cache = ctx.query_cache().expect("first warm query_cache");
            create_marker(&cache);
        }

        // Panic on another thread WHILE holding the cache guard: its `Drop` runs
        // during unwind and bumps the slot's invalidation floor past this
        // generation.
        let ctx2 = Arc::clone(&ctx);
        let handle = std::thread::spawn(move || {
            ctx2.begin_request().expect("begin_request on panic thread");
            let _cache = ctx2.query_cache().expect("query_cache on panic thread");
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
                .query_cache()
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
        let cache = ctx.query_cache().expect("query_cache request 3");
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

        // First request opens generation 1.
        ctx.begin_request().expect("begin_request");
        {
            let cache = ctx.query_cache().expect("first warm query_cache");
            assert_eq!(doc_count(&cache), 3);
        }
        assert_eq!(ctx.generation_opens(), 1, "cold start is one open");

        // Invalidate generation 1 for EVERY thread (corruption-class error →
        // floor bump). No config change, no rebuild, no inode shift.
        let corrupt = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            Some("database disk image is malformed".to_string()),
        );
        ctx.note_tool_error(&anyhow::Error::new(corrupt).context("tool body failed"));

        const N: usize = 8;
        let barrier = Arc::new(Barrier::new(N));
        let mut handles = Vec::with_capacity(N);
        for _ in 0..N {
            let ctx = Arc::clone(&ctx);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                ctx.begin_request().expect("begin_request");
                let cache = ctx.query_cache().expect("query_cache");
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
            let cache = ctx.query_cache().expect("first warm query_cache");
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
            let cache = ctx.query_cache().expect("second warm query_cache");
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
}
