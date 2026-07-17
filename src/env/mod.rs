//! The vault-env seam: a surface-neutral construction and freshness contract
//! for anything that needs a live, trustworthy handle onto a vault. MCP (stdio
//! `norn mcp`) and the `norn serve` daemon are CONSUMERS of this seam, not its
//! owners — today they are the only two, but neither is baked into the
//! contract itself.
//!
//! # Two modes: cold (one-shot processes) and warm (resident processes)
//!
//! [`VaultEnv`] holds a parsed [`LoadedConfig`] and hands out a cache handle
//! per call via [`VaultEnv::query_cache`]. It has two modes, chosen at
//! construction, that differ in how much they hold open across calls.
//!
//! ## Cold mode — [`VaultEnv::open`] (one-shot processes; today the CLI-equivalent stdio path)
//!
//! Config is parsed once at startup and held for the process lifetime; a config
//! change requires a process restart, exactly like the CLI re-reads config fresh
//! on every invocation. The cache is deliberately **not** held: each call
//! opens a fresh [`Cache`] via `open_for_query` — the TRUSTING open (NRN-275),
//! which skips `PRAGMA integrity_check` and pays only the cheap incremental
//! freshness refresh, exactly like the direct CLI. Byte corruption is caught by
//! rebuild-on-corruption (the read helpers evict + rebuild + retry once) rather
//! than a per-open scan. This matches the CLI's per-invocation behavior exactly
//! and needs no filesystem watcher.
//!
//! ## Warm mode — [`VaultEnv::open_warm`] (resident processes; the `norn serve` daemon)
//!
//! A long-lived process holds one `VaultEnv` open across many calls, so
//! re-opening and re-refreshing on every call is wasteful. Warm mode instead
//! **verifies trust once** (the first `integrity_check` when the cache is first
//! opened) and then **continuously maintains** it with a cheap per-request
//! self-heal pipeline, upholding the ADR-0005 trust invariant: reading through
//! norn must always feel like touching the actual files.
//!
//!
//! Warm mode is only ever constructed with the default config location; the
//! daemon wire never carries a custom `--config` path, so `open_warm` takes only
//! `cwd` and hard-codes `config_path = None`.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::fs::File;
use std::ops::{Deref, DerefMut};
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};

use crate::cache::command::open_for_query;
use crate::cache::{
    cache_dir_for, Cache, CacheError, ChangeDetectOptions, Freshness, FreshnessProbe,
    StatSweepProbe,
};
use crate::config_loader::{load_config, LoadedConfig};
use crate::mcp::writer_queue::{
    ChunkOutcome, Handle, Outcome, ValidityGuard, WriterProgressState, WriterQueue,
};

mod ensure;
mod error;
mod generation;
mod refresh;
mod request_scope;
#[cfg(test)]
mod tests;

// Explicit hub imports: exactly what this file's own code needs, plus every
// name a submodule's `use super::*;` resolves through this module (the
// submodules stay tightly coupled via glob imports of the hub — see their
// individual `use super::*;` — so anything one submodule references from
// another must be named here).
use ensure::{device_inode, generation_still_current_guard};
use error::WarmContextError;
use generation::{
    open_companion_on_inode, read_pool_cap, CacheHandle, Generation, GrowParams, IndexIdentity,
    ReadPool, SharedSlot, WarmGuard, WarmSlot,
};
#[cfg(test)]
use refresh::TestGate;
use refresh::{config_yaml_path, fingerprint_config, ConfigFingerprint, RefreshTicket};

// Types defined in submodules but part of the crate-internal surface other
// modules import via `crate::env::*` — re-exported at their original visibility.
#[cfg(test)]
pub(crate) use generation::ReadPoolCapGuard;
pub(crate) use request_scope::RequestScope;

/// Cold (stdio) vs warm (daemon) behavior for `query_cache`.
// The warm variant is naturally large (it owns the held cache state); exactly
// one `Mode` exists per long-lived `VaultEnv`, so the size gap is irrelevant.
#[allow(clippy::large_enum_variant)]
enum Mode {
    Cold,
    // Constructed by `open_warm`, used by the unix-only `norn serve` daemon
    // (`src/serve/`); dead on non-unix builds where the daemon can't run.
    #[cfg_attr(not(unix), allow(dead_code))]
    Warm(WarmSlot),
}

/// The vault-env seam's handle: a cold or warm binding onto one vault, held by
/// whichever consumer constructed it (MCP, `norn serve`, or a future caller).
///
/// Holds a parsed [`LoadedConfig`] behind a `Mutex<Arc<..>>` (warm mode swaps in
/// a re-parsed config on a config edit; cold mode never mutates it) and a
/// [`Mode`] that selects the per-call cache strategy. In warm mode the cache
/// binding itself is NOT interior-mutable — it is an immutable `Arc<Generation>`
/// swapped as a whole. See the module docs for the full design.
pub(crate) struct VaultEnv {
    /// Absolute path to the vault root, as passed via `--cwd`.
    pub(crate) vault_root: Utf8PathBuf,
    /// Parsed and compiled config. Behind a `Mutex<Arc<..>>` so warm mode can
    /// atomically swap in a re-parsed config without disturbing readers holding
    /// a cloned `Arc`. This is the config `Arc` a request binds at its boundary.
    config: Mutex<Arc<LoadedConfig>>,
    mode: Mode,
    /// Cold-mode only: skip the per-call incremental cache refresh, honoring a
    /// one-shot `--no-cache-refresh` (NRN-291). Always `false` in warm mode (the
    /// daemon continuously maintains freshness) and in the stdio `norn mcp`
    /// server (which always refreshes per call). Only [`VaultEnv::open_cold`]
    /// sets it non-`false`, and only [`query_cache`](Self::query_cache)'s cold
    /// branch reads it.
    cold_no_cache_refresh: bool,
}

impl VaultEnv {
    /// Open a COLD vault context (stdio `norn mcp`). Reads and parses the config
    /// once; fails fast if the config file exists but is unreadable/malformed.
    ///
    /// A missing config file is not an error — `load_config` returns
    /// `LoadedConfig::default()` when no `.norn/config.yaml` is found, so the
    /// server starts cleanly against unconfigured vaults.
    pub(crate) fn open(cwd: &Utf8Path, config_path: Option<&Utf8PathBuf>) -> Result<Self> {
        Self::open_cold(cwd, config_path, false)
    }

    /// Open a COLD vault context that also honors a one-shot `--no-cache-refresh`
    /// (NRN-291). Identical to [`open`](Self::open) except the flag threads into
    /// [`query_cache`](Self::query_cache)'s cold branch, so the generic dispatch
    /// (`crate::dispatch`) reproduces the pre-NRN-291 direct path's
    /// `cache::command::load_graph_index(..., no_cache_refresh)` behavior when a
    /// forced-direct `--no-cache-refresh` repair executes locally.
    pub(crate) fn open_cold(
        cwd: &Utf8Path,
        config_path: Option<&Utf8PathBuf>,
        no_cache_refresh: bool,
    ) -> Result<Self> {
        let config = load_config(&cwd.to_path_buf(), config_path)?;
        Ok(Self {
            vault_root: cwd.to_path_buf(),
            config: Mutex::new(Arc::new(config)),
            mode: Mode::Cold,
            cold_no_cache_refresh: no_cache_refresh,
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
    #[cfg(test)]
    pub(crate) fn open_warm(cwd: &Utf8Path) -> Result<Self> {
        Self::open_warm_with_progress(cwd, Arc::new(WriterProgressState::default()))
    }

    /// Daemon constructor that binds the queue to progress retained across
    /// context eviction and recreation for this vault.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn open_warm_with_progress(
        cwd: &Utf8Path,
        progress: Arc<WriterProgressState>,
    ) -> Result<Self> {
        let config = load_config(&cwd.to_path_buf(), None)?;
        let config_fp = fingerprint_config(&config_yaml_path(cwd))?;
        Ok(Self {
            vault_root: cwd.to_path_buf(),
            config: Mutex::new(Arc::new(config)),
            cold_no_cache_refresh: false,
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
                queue: WriterQueue::spawn_with_progress(cwd.as_str(), progress),
            }),
        })
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

    /// Whether this is a COLD (stdio `norn mcp`) context. Cold mode still needs the
    /// server's `call_lock` to serialize its per-call cold cache opens (the NRN-55
    /// cold-open DDL-race guard); warm mode retired that lock (NRN-253) because
    /// every per-request dependency is now per-request state or structurally
    /// synchronized. The server (`mcp::server`) branches on this to acquire the lock
    /// only when cold.
    pub(crate) fn is_cold(&self) -> bool {
        matches!(self.mode, Mode::Cold)
    }

    /// Open a query cache for one tool call. Serves both modes with the same call
    /// shape (`let cache = ctx.query_cache()?;`) via a `Deref`/`DerefMut`-into-
    /// `Cache` handle, so tool code does not fork on mode.
    ///
    /// - Cold: opens a fresh [`Cache`] via `open_for_query` (the trusting open +
    ///   incremental refresh every call; rebuild-on-corruption, no per-open
    ///   integrity_check — NRN-275), exactly like the direct CLI.
    /// - Warm: runs the verify-once + per-request self-heal pipeline (see module
    ///   docs) and hands out an owned guard into the bound generation's connection.
    pub(crate) fn query_cache(&self, scope: &RequestScope) -> Result<CacheHandle> {
        match &self.mode {
            Mode::Cold => {
                let config = scope.config();
                let cache = open_for_query(
                    &self.vault_root,
                    &config.index_options,
                    self.cold_no_cache_refresh,
                )?;
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
    ///   (the trusting open + incremental refresh; NRN-275), and the reader
    ///   reconstructs the index from it — the exact sequence
    ///   `cache::command::load_graph_index` runs
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
    /// request reopens and re-verifies).
    /// The source of truth remains the Markdown files: a plan built from a
    /// corrupt index is caught by the apply-time snapshot checks or surfaces as
    /// an error that triggers the eviction path. Cold (stdio `norn mcp`) and
    /// direct (non-daemon) invocations no longer verify per call (NRN-275): they
    /// open TRUSTING and rely on rebuild-on-corruption instead.
    ///
    /// Config freshness / root-liveness (steps 0–1) already ran in
    /// [`begin_request`](Self::begin_request) at the per-request seam, exactly as
    /// for `query_cache`, so config is stable for the whole request.
    pub(crate) fn load_graph_index(&self, scope: &RequestScope) -> Result<crate::core::GraphIndex> {
        let cache = self.query_cache(scope)?;
        cache.load_graph_index()
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
}
