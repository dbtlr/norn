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
//! opens a fresh [`Cache`] via `open_for_query` — the TRUSTING open (NRN-275),
//! which skips `PRAGMA integrity_check` and pays only the cheap incremental
//! freshness refresh, exactly like the direct CLI. Byte corruption is caught by
//! rebuild-on-corruption (the read helpers evict + rebuild + retry once) rather
//! than a per-open scan. This matches the CLI's per-invocation behavior exactly
//! and needs no filesystem watcher.
//!
//! ## Warm mode — [`VaultContext::open_warm`] (daemon `norn serve`)
//!
//! A long-lived daemon holds one `VaultContext` open across many requests, so
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

use ensure::*;
use error::*;
use generation::*;
use refresh::*;

// Types defined in submodules but part of the crate-internal surface other
// modules import via `crate::env::*` — re-exported at their original visibility.
#[cfg(test)]
pub(crate) use generation::ReadPoolCapGuard;
pub(crate) use request_scope::RequestScope;

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

    /// The current warm generation number (`0` if none) — a monotonic reopen
    /// probe. Replaces the pre-NRN-253 `warm_marker` TEMP-table probe: pooled read
    /// connections are `query_only`, so `CREATE TEMP TABLE` no longer works as a
    /// same-connection marker. Same number across two requests ⇒ the generation
    /// (and its pooled connections) was reused; a higher number ⇒ it was reopened.
    /// Monotonic generation numbers can't false-positive the way a recycled
    /// connection pointer could, so this is a strictly more robust reopen proof.
    fn gen_number(ctx: &VaultContext) -> u64 {
        ctx.current_generation().map(|g| g.number).unwrap_or(0)
    }

    /// The raw `sqlite3*` pointer behind a cache's connection — a per-connection
    /// identity used to prove two checkouts are DISTINCT connections and that a
    /// sequential checkout REUSES one. Safe here: the pointer is only compared as an
    /// integer, never dereferenced, and the connections it names are kept alive for
    /// the comparison.
    fn conn_ptr(cache: &Cache) -> usize {
        // SAFETY: `handle()` returns the live `sqlite3*`; we only read it as an
        // address for identity comparison and never dereference it.
        (unsafe { cache.conn().handle() }) as usize
    }

    /// Plant per-connection TEMP objects (a read-shadow view / triggers) on a
    /// pooled connection for the verify-once proofs below. Pooled connections are
    /// `query_only` (NRN-253), which blocks `CREATE TEMP …`, so this briefly toggles
    /// query_only OFF to create the TEMP objects and restores it — the objects
    /// persist on the connection regardless of query_only, and reads through them
    /// are unaffected. This preserves the pre-NRN-253 shadow mechanism exactly; only
    /// the planting gains the toggle (the assertions are unchanged).
    fn plant_temp_on_pooled(cache: &Cache, batch: &str) {
        cache.conn().execute_batch("PRAGMA query_only=OFF").unwrap();
        cache.conn().execute_batch(batch).unwrap();
        cache.conn().execute_batch("PRAGMA query_only=ON").unwrap();
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

    /// Warm reuse / verify-once: two sequential warm calls share one pooled
    /// connection. The first call's checkout is returned to the pool on drop, so the
    /// second call pops the SAME connection — proven by an unchanged `sqlite3*`
    /// pointer (the seed is never freed, so the address can't be recycled) — and the
    /// captured `(dev, ino)` identity is unchanged.
    #[test]
    fn warm_reuses_one_connection_across_calls() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm should succeed");

        let ptr1 = {
            ctx.begin_request().expect("begin_request");
            let cache = ctx.query_cache_unscoped().expect("first warm query_cache");
            conn_ptr(&cache)
        };
        let id1 = ctx.warm_db_identity();
        assert!(id1.is_some(), "warm state should be held after first call");

        let ptr2 = {
            ctx.begin_request().expect("begin_request");
            let cache = ctx.query_cache_unscoped().expect("second warm query_cache");
            conn_ptr(&cache)
        };
        assert_eq!(
            ptr1, ptr2,
            "the same pooled connection must be reused across warm calls (verify-once)"
        );
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
    /// forces a reopen — the generation number advances, the identity changed, and
    /// the rebuilt cache still serves the vault.
    #[test]
    fn warm_self_heals_when_cache_db_disappears() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm should succeed");

        {
            ctx.begin_request().expect("begin_request");
            let cache = ctx.query_cache_unscoped().expect("first warm query_cache");
            assert_eq!(doc_count(&cache), 3);
        }
        let g1 = gen_number(&ctx);
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
                gen_number(&ctx) > g1,
                "generation number must advance ⇒ the connection was reopened"
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
        // The generation-number advance above is the reopen proof.
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
            assert_eq!(
                archived_count(&cache),
                1,
                "archived doc indexed before ignore"
            );
        }
        let g1 = gen_number(&ctx);

        write_config(&root, "files:\n  ignore:\n    - \"Archive/**\"\n");

        {
            ctx.begin_request().expect("begin_request");
            let cache = ctx
                .query_cache_unscoped()
                .expect("second warm query_cache after files.ignore change");
            assert!(
                gen_number(&ctx) > g1,
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
            let _cache = ctx.query_cache_unscoped().expect("first warm query_cache");
        }
        let g1 = gen_number(&ctx);

        write_config(&root, "validate:\n  ignore:\n    - \"logs/**\"\n");

        {
            ctx.begin_request().expect("begin_request");
            let _cache = ctx
                .query_cache_unscoped()
                .expect("second warm query_cache after non-index config change");
            assert_eq!(
                gen_number(&ctx),
                g1,
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
            let _cache = ctx.query_cache_unscoped().expect("first warm query_cache");
        }
        let g1 = gen_number(&ctx);
        assert!(ctx.config().index_options.alias_field.is_none());

        // Index-relevant: alias_field feeds Cache::open_with_index.
        write_config(&root, "links:\n  alias_field: aliases\n");

        {
            ctx.begin_request().expect("begin_request");
            let _cache = ctx
                .query_cache_unscoped()
                .expect("second warm query_cache after index-relevant config change");
            assert!(
                gen_number(&ctx) > g1,
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
    /// (`cache::command::load_graph_index`, integrity_check every call) on the same
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
        let cold =
            crate::cache::command::load_graph_index(&root, &cold_config.index_options, false)
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
    /// (Red-proofed: pointing the warm arm back at `cache::command::load_graph_index`
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
            plant_temp_on_pooled(
                &cache,
                "CREATE TEMP VIEW headings AS SELECT * FROM main.headings WHERE 0",
            );
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
        let cold =
            crate::cache::command::load_graph_index(&root, &cold_config.index_options, false)
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
            plant_temp_on_pooled(
                &cache,
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
            );
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
        let cold =
            crate::cache::command::load_graph_index(&root, &cold_config.index_options, false)
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
            let _cache = ctx.query_cache_unscoped().expect("first warm query_cache");
        }
        let g1 = gen_number(&ctx);
        assert!(ctx.config().index_options.alias_field.is_none());

        // Index-relevant change.
        write_config(&root, "links:\n  alias_field: aliases\n");

        // ONE request, in a tool's access order: begin_request, THEN read config,
        // THEN open the cache.
        ctx.begin_request()
            .expect("begin_request after config change");
        let config_alias = ctx.config().index_options.alias_field.clone();
        let _cache = ctx
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
            gen_number(&ctx) > g1,
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
            let _cache = ctx.query_cache(&scope).expect("first warm query_cache");
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

        // Next request reopens a NEW generation — the corruption bumped the
        // invalidation floor past generation 1.
        ctx.begin_request().expect("begin_request after eviction");
        let cache = ctx
            .query_cache_unscoped()
            .expect("query_cache after eviction");
        assert!(
            gen_number(&ctx) > gen1.number,
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
            let _cache = ctx.query_cache_unscoped().expect("first warm query_cache");
        }
        let g1 = gen_number(&ctx);

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

        // The first post-panic request (request 2) must recover: a rebuilt
        // generation (number advanced past g1). Capture it so request 3 below can
        // prove the generation this request opened is REUSED, not re-evicted.
        ctx.begin_request().expect("begin_request after panic");
        {
            let cache = ctx
                .query_cache_unscoped()
                .expect("query_cache must recover from a panic-invalidated generation");
            assert!(
                gen_number(&ctx) > g1,
                "warm state must be rebuilt after panic recovery"
            );
            assert_eq!(doc_count(&cache), 3);
        }
        let g2 = gen_number(&ctx);

        // The floor bump must be one-shot, not sticky: request 3 must reuse the
        // SAME generation request 2 rebuilt (number unchanged), proving the
        // recovery does not keep re-evicting on every subsequent request.
        ctx.begin_request().expect("begin_request request 3");
        let _cache = ctx.query_cache_unscoped().expect("query_cache request 3");
        assert_eq!(
            gen_number(&ctx),
            g2,
            "a second post-panic request must reuse the recovered generation, \
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
    /// fd) release only when its last request-bound [`CacheHandle`] drops. The
    /// handle returned by `query_cache` pins N across a reopen to an observably
    /// different N+1, continues serving N's snapshot, and releases N when dropped.
    #[test]
    fn warm_reopen_drains_and_drops_prior_generation() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");

        let scope1 = ctx.begin_request().expect("begin_request");
        let gen1_cache = ctx.query_cache(&scope1).expect("first warm query_cache");
        assert_eq!(doc_count(&gen1_cache), 3);

        let gen1 = ctx.current_generation().expect("generation 1 held");
        assert_eq!(gen1.number, 1);
        let weak = Arc::downgrade(&gen1);
        drop(gen1);

        // Make the rebuilt generation observably different from the request's
        // bound snapshot before forcing the ground-shift.
        std::fs::write(
            root.join("delta.md"),
            "---\ntype: note\nstatus: active\n---\nDelta body\n",
        )
        .unwrap();

        // Trigger a reopen via ground-shift: remove the cache dir out from under
        // the live context. The request-bound handle keeps gen1's unlinked db
        // alive (POSIX ghost), so it can still serve its snapshot after this.
        let (_canonical, cache_dir) = cache_dir_for(&root).expect("cache_dir_for");
        std::fs::remove_dir_all(cache_dir.as_std_path()).expect("remove cache dir");

        let scope2 = ctx.begin_request().expect("begin_request");
        {
            let cache = ctx.query_cache(&scope2).expect("second warm query_cache");
            assert_eq!(
                doc_count(&cache),
                4,
                "generation N+1 serves the rebuilt four-document vault"
            );
        }
        let gen2 = ctx.current_generation().expect("generation 2 held");
        assert_eq!(gen2.number, 2, "reopen advanced the generation number");
        drop(gen2);

        assert_eq!(
            doc_count(&gen1_cache),
            3,
            "the request-bound N handle still serves its old three-document snapshot"
        );

        assert!(
            weak.upgrade().is_some(),
            "generation N remains alive while its request-bound handle is held"
        );

        // Dropping the request's handle releases N entirely (connection + sentinel
        // fd close via Drop) — the Weak going dead is the observable Drop effect.
        drop(gen1_cache);
        assert!(
            weak.upgrade().is_none(),
            "generation N is fully dropped after its request-bound handle releases it"
        );
    }

    // ---- Read pool (NRN-253) ------------------------------------------------

    /// Two concurrently-held checkouts are DISTINCT connections: the first pops the
    /// seed, the second lazily grows a second connection (proven by distinct
    /// `sqlite3*` pointers and a grow-open count of exactly 1).
    #[test]
    fn two_concurrent_checkouts_are_distinct_connections() {
        // Force a cap of at least 2 so growth is possible even on a single-core
        // host (default cap = min(8, parallelism) could be 1 there). The shared
        // guard serializes the cap override against the `server` suite too.
        let _cap = ReadPoolCapGuard::pin(8);

        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");
        ctx.begin_request().expect("begin_request");
        {
            let _c = ctx.query_cache_unscoped().expect("warm up");
        }
        let gen = ctx.current_generation().expect("generation held");

        // Hold BOTH checkouts at once: the pool cannot hand the same connection
        // twice, so it must grow a second.
        let c1 = gen.test_read_conn();
        let c2 = gen.test_read_conn();
        assert_ne!(
            conn_ptr(&c1),
            conn_ptr(&c2),
            "two concurrently-held checkouts must be distinct connections"
        );
        assert_eq!(
            gen.read_pool.grow_opens.load(Ordering::Relaxed),
            1,
            "exactly one connection was lazily grown beyond the seed"
        );
    }

    /// Sequential checkout/drop/checkout REUSES the pooled connection: the seed
    /// returns to the pool on drop and is popped again — same `sqlite3*` pointer, no
    /// lazy-grow open.
    #[test]
    fn sequential_checkout_reuses_pooled_connection() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");
        ctx.begin_request().expect("begin_request");
        {
            let _c = ctx.query_cache_unscoped().expect("warm up");
        }
        let gen = ctx.current_generation().expect("generation held");
        assert_eq!(
            gen.read_pool.grow_opens.load(Ordering::Relaxed),
            0,
            "only the seed exists before any concurrent checkout"
        );

        let ptr1 = {
            let c = gen.test_read_conn();
            conn_ptr(&c)
        };
        let ptr2 = {
            let c = gen.test_read_conn();
            conn_ptr(&c)
        };
        assert_eq!(
            ptr1, ptr2,
            "sequential checkout/drop/checkout must reuse the same pooled connection"
        );
        assert_eq!(
            gen.read_pool.grow_opens.load(Ordering::Relaxed),
            0,
            "reuse must not lazily grow a new connection"
        );
    }

    /// Wait-at-cap: with the cap forced to 1, a second checkout BLOCKS while the
    /// only connection is held, then proceeds once it is returned. Channel-gated,
    /// no sleeps in the success path.
    #[test]
    fn checkout_waits_at_cap_until_a_connection_returns() {
        let _cap = ReadPoolCapGuard::pin(1);

        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");
        ctx.begin_request().expect("begin_request");
        {
            let _c = ctx.query_cache_unscoped().expect("warm up");
        }
        let gen = ctx.current_generation().expect("generation held");

        // Hold the ONLY connection (cap == 1).
        let held = gen.test_read_conn();

        let gen_waiter = Arc::clone(&gen);
        let (got_tx, got_rx) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let c = gen_waiter.test_read_conn(); // blocks until `held` returns
            got_tx.send(()).unwrap();
            drop(c);
        });

        // The waiter must be blocked: it cannot have acquired a connection while
        // the only one is still held.
        assert!(
            got_rx
                .recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "second checkout must block while the only connection is held"
        );

        // Return the held connection; the waiter now proceeds.
        drop(held);
        got_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("blocked checkout must proceed once a connection is returned");
        waiter.join().expect("waiter thread panicked");
    }

    /// A grow FAILURE degrades to waiting, never fails the read (NRN-253 review):
    /// with the seed checked out and growth broken (grow params pointing at a
    /// nonexistent vault), a second checkout blocks — where it previously returned
    /// the grow error, failing a read that succeeded pre-pool (e.g. the inode-swap
    /// bail during an external `cache clear`) — and proceeds, error-free, once the
    /// seed checks back in.
    #[test]
    fn checkout_grow_failure_degrades_to_waiting_for_a_checkin() {
        let (_tmp, root) = make_seeded_vault();
        let seed = Cache::open(&root).expect("seed cache");
        // Broken on purpose: a lazy grow against a nonexistent root always fails.
        let grow = GrowParams {
            vault_root: root.join("does-not-exist"),
            identity: IndexIdentity {
                index_set_hash: String::new(),
                alias_field: None,
                ignore: Vec::new(),
            },
            index_set: BTreeSet::new(),
            expected_identity: (0, 0),
        };
        let pool = ReadPool::seed(seed, grow, 2).expect("seed pool");

        // Hold the seed. Capacity remains (cap 2, total 1), so the concurrent
        // checkout below attempts — and fails — the lazy grow.
        let held = pool.checkout();

        let pool_waiter = Arc::clone(&pool);
        let (got_tx, got_rx) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let c = pool_waiter.checkout(); // grow fails → degrades to waiting
            got_tx.send(()).unwrap();
            drop(c);
        });

        // The grower must be BLOCKED (not errored): nothing is idle yet.
        assert!(
            got_rx
                .recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "a failed grow must degrade the checkout to waiting, not fail it"
        );

        // Check the seed back in; the degraded waiter now proceeds, no error
        // surfaced anywhere.
        drop(held);
        got_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the degraded checkout must proceed once the seed checks in");
        waiter.join().expect("waiter thread panicked");
        assert_eq!(
            pool.grow_opens.load(Ordering::Relaxed),
            0,
            "no connection was ever grown through the broken params"
        );
    }

    /// query_only enforcement: a write attempted through a pooled connection fails
    /// with a `SQLITE_READONLY`-class error.
    #[test]
    fn pooled_connection_is_query_only() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");
        ctx.begin_request().expect("begin_request");
        let cache = ctx.query_cache_unscoped().expect("query_cache");

        let err = cache
            .conn()
            .execute("CREATE TABLE nope (x INTEGER)", [])
            .expect_err("a write through a query_only pooled connection must fail");
        match err {
            rusqlite::Error::SqliteFailure(e, _) => assert_eq!(
                e.code,
                rusqlite::ErrorCode::ReadOnly,
                "expected SQLITE_READONLY, got {e:?}"
            ),
            other => panic!("expected a SqliteFailure/ReadOnly error, got {other:?}"),
        }
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
    /// completes even while a thread holds a checked-out pooled READ connection. If
    /// the refresh still touched the read connection it would block on the held
    /// checkout forever (deadlock) and the timeout would fire.
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

        // Hold a checked-out pooled READ connection on THIS thread.
        let read_guard = gen.test_read_conn();

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
            let _cache = ctx.query_cache_unscoped().expect("first warm query_cache");
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

        // The next request reopens a NEW generation (number bumped).
        ctx.begin_request().expect("begin_request after eviction");
        let cache = ctx
            .query_cache_unscoped()
            .expect("query_cache after eviction");
        assert!(
            gen_number(&ctx) > gen1.number,
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
        let _cap = ReadPoolCapGuard::pin(1);
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

    /// Atomic publication: every staging boundary exposes the entire old main
    /// snapshot; only the terminal short transaction switches readers to new.
    #[test]
    fn increment_publishes_all_files_atomically_after_staging() {
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

        // Boundary 1: alpha is staged, but main is still entirely old.
        reached.recv().expect("boundary 1");
        {
            let cache = generation.test_read_conn();
            assert!(
                body_of(&cache, "alpha.md")
                    .as_deref()
                    .is_some_and(|b| b.contains("Alpha body")),
                "alpha must remain old while staging"
            );
            assert!(
                !doc_present(&cache, "delta.md"),
                "delta must remain absent while staging"
            );
        }
        release.send(()).unwrap();

        // Boundary 2: all document rows are staged; main is still old.
        reached.recv().expect("boundary 2");
        {
            let cache = generation.test_read_conn();
            assert!(
                body_of(&cache, "alpha.md")
                    .as_deref()
                    .is_some_and(|b| b.contains("Alpha body")),
                "alpha must remain old until terminal publication"
            );
            assert!(
                !doc_present(&cache, "delta.md"),
                "delta must remain absent until terminal publication"
            );
        }
        release.send(()).unwrap();

        // Boundary 3 is the explicit ready-to-publish yield. Even here readers
        // still see the complete old snapshot.
        reached.recv().expect("ready-to-publish boundary");
        {
            let cache = generation.test_read_conn();
            assert!(body_of(&cache, "alpha.md")
                .as_deref()
                .is_some_and(|b| b.contains("Alpha body")));
            assert!(!doc_present(&cache, "delta.md"));
        }
        release.send(()).unwrap();

        assert!(
            matches!(handle.wait(), Outcome::Done(Ok(()))),
            "the increment commit must complete cleanly"
        );
        {
            let cache = generation.test_read_conn();
            assert!(
                body_of(&cache, "alpha.md")
                    .as_deref()
                    .is_some_and(|b| b.contains("REVISED")),
                "terminal publication switches alpha to new"
            );
            assert!(
                doc_present(&cache, "delta.md"),
                "terminal publication adds delta in the same snapshot"
            );
        }
        assert_eq!(
            ctx.detect_change_count(),
            0,
            "a completed increment leaves detect empty"
        );
    }

    /// Reservation closes the pre-first-chunk authority window: a successful
    /// refresh on the SAME dedicated writer connection after parsing invalidates
    /// the reservation, so the older driver completes without publishing.
    #[test]
    fn refresh_between_increment_parse_and_first_chunk_supersedes_reservation() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");
        ctx.begin_request().expect("begin_request");
        {
            let cache = ctx.query_cache_unscoped().expect("query_cache");
            assert_eq!(doc_count(&cache), 3);
        }
        let generation = ctx.current_generation().expect("current generation");
        let Mode::Warm(slot) = &ctx.mode else {
            unreachable!()
        };

        let baseline = generation
            .checkout_read()
            .load_graph_index()
            .expect("baseline load");
        let fingerprint = crate::cache::graph_fingerprint(&baseline);
        let reservation = match ctx
            .submit_increment_reservation(slot, &generation, fingerprint)
            .wait()
        {
            Outcome::Done(Ok(reservation)) => reservation,
            other => panic!("reservation failed: {other:?}"),
        };
        std::fs::write(
            root.join("alpha.md"),
            "---\ntype: note\n---\nOLDER PARSED BODY\n",
        )
        .unwrap();
        let commit = crate::cache::Cache::begin_increment_commit(
            &root,
            &["alpha.md".into()],
            generation.index_identity.alias_field.as_deref(),
            &generation.index_identity.ignore,
            &reservation,
            baseline,
        )
        .expect("parse older increment");

        std::fs::write(
            root.join("alpha.md"),
            "---\ntype: note\n---\nNEWER REFRESH BODY\n",
        )
        .unwrap();
        assert!(matches!(ctx.test_refresh_current(), RefreshOutcome::Served));

        assert!(matches!(
            ctx.submit_increment_commit(slot, &generation, commit)
                .wait(),
            Outcome::Done(Ok(()))
        ));
        let cache = generation.test_read_conn();
        assert!(
            body_of(&cache, "alpha.md")
                .as_deref()
                .is_some_and(|body| body.contains("NEWER REFRESH BODY")),
            "the older parsed driver must not overwrite the newer refresh"
        );
    }

    #[test]
    fn refresh_waiters_succeed_when_post_refresh_temp_cleanup_fails() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");
        ctx.begin_request().expect("begin_request");
        let _cache = ctx.query_cache_unscoped().expect("build cache");
        drop(_cache);
        let generation = ctx.current_generation().expect("current generation");
        let Mode::Warm(slot) = &ctx.mode else {
            unreachable!()
        };
        let baseline = generation
            .checkout_read()
            .load_graph_index()
            .expect("baseline load");
        let fingerprint = crate::cache::graph_fingerprint(&baseline);
        let _reservation = match ctx
            .submit_increment_reservation(slot, &generation, fingerprint)
            .wait()
        {
            Outcome::Done(Ok(reservation)) => reservation,
            other => panic!("reservation failed: {other:?}"),
        };
        {
            let cache = generation
                .write_cache
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            cache
                .conn
                .execute_batch(
                    "CREATE TEMP TRIGGER fail_refresh_cleanup
                     BEFORE DELETE ON norn_increment_jobs
                     BEGIN SELECT RAISE(FAIL, 'cleanup failed'); END;",
                )
                .unwrap();
        }
        std::fs::write(root.join("delta.md"), "---\n---\nDelta\n").unwrap();
        assert!(matches!(ctx.test_refresh_current(), RefreshOutcome::Served));
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

        // Explicit boundary immediately before terminal publication.
        reached.recv().expect("ready-to-publish boundary");
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
        let baseline = {
            let cache = ctx.query_cache(&scope).expect("query_cache");
            assert_eq!(doc_count(&cache), 3);
            cache.load_graph_index().expect("pre-apply baseline")
        };
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
            ctx_worker.commit_apply_increments(&scope, &changed_worker, baseline);
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

        // Staging was invisible and generation death prevents terminal
        // publication, so neither addition appears in the dead generation.
        {
            let cache = generation.test_read_conn();
            assert!(
                !doc_present(&cache, "delta.md"),
                "staged delta must never become visible after generation death"
            );
            assert!(
                !doc_present(&cache, "epsilon.md"),
                "epsilon must remain unpublished after generation death"
            );
        }

        // The degrade left the both-surfaces operator note (drained above).
        assert!(
            notes.iter().any(|n| n.contains("abandoned")),
            "a dropped increment must leave the deferred operator note, got {notes:?}"
        );
        assert_eq!(
            generation
                .write_cache
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .staged_increment_job_count(),
            0,
            "dropped bulk must schedule reservation cleanup on the held generation"
        );

        // A later request opens a healthy generation and heals from disk.
        ctx.begin_request()
            .expect("begin request after invalidation");
        let healed = ctx.query_cache_unscoped().expect("healed cache");
        assert!(doc_present(&healed, "delta.md"));
        assert!(doc_present(&healed, "epsilon.md"));
    }

    #[test]
    fn panicked_increment_cleans_its_reserved_temp_job() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");
        let scope = ctx.begin_request().expect("begin_request");
        let baseline = ctx
            .query_cache(&scope)
            .expect("build cache")
            .load_graph_index()
            .expect("pre-apply baseline");
        let generation = ctx.current_generation().expect("current generation");
        generation
            .inject_increment_panic
            .store(true, Ordering::Release);
        std::fs::write(root.join("delta.md"), "---\n---\nDelta\n").unwrap();

        ctx.commit_apply_increments(&scope, &["delta.md".into()], baseline);

        assert!(scope
            .take_operator_notes()
            .iter()
            .any(|note| note.contains("panicked")));
        assert_eq!(
            generation
                .write_cache
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .staged_increment_job_count(),
            0,
            "panicked bulk must schedule reservation cleanup"
        );
    }

    #[test]
    fn affected_source_drift_degrades_post_apply_increment() {
        std::env::set_var("NORN_CACHE_INCREMENT_BUDGET_MS", "0");
        let (_tmp, root) = make_seeded_vault();
        let ctx = Arc::new(VaultContext::open_warm(&root).expect("open_warm"));
        let scope = ctx.begin_request().expect("begin_request");
        let baseline = ctx
            .query_cache(&scope)
            .expect("build cache")
            .load_graph_index()
            .expect("pre-apply baseline");
        let generation = ctx.current_generation().expect("current generation");
        std::fs::write(root.join("alpha.md"), "---\n---\nPARSED ALPHA\n").unwrap();
        let (reached, release) = ctx.install_increment_gate(&generation);
        let worker_ctx = Arc::clone(&ctx);
        let worker = std::thread::spawn(move || {
            worker_ctx.commit_apply_increments(&scope, &["alpha.md".into()], baseline);
            scope.take_operator_notes()
        });
        reached.recv().expect("first staging boundary");
        std::fs::write(root.join("alpha.md"), "---\n---\nDRIFTED ALPHA\n").unwrap();
        release.send(()).unwrap();
        reached.recv().expect("ready boundary");
        release.send(()).unwrap();
        let notes = worker.join().unwrap();
        assert!(
            notes.iter().any(|note| note.contains("failed")),
            "affected drift must produce the deferred-cache note: {notes:?}"
        );
    }

    /// A request's pre-apply graph belongs to the generation it bound. If a
    /// concurrent reopen advances `slot.current` before the post-apply increment,
    /// that old graph must never be overlaid onto the newer generation.
    #[test]
    fn increment_with_old_bound_baseline_does_not_target_new_generation() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");

        let scope = ctx.begin_request().expect("begin_request");
        let baseline = ctx
            .query_cache(&scope)
            .expect("generation 1 cache")
            .load_graph_index()
            .expect("generation 1 baseline");
        assert_eq!(scope.bound_generation(), 1);

        ctx.test_invalidate_current_generation();
        {
            let _newer = ctx.query_cache_unscoped().expect("open generation 2");
        }
        let generation_2 = ctx.current_generation().expect("generation 2 current");
        assert_eq!(generation_2.number, 2);

        std::fs::write(root.join("delta.md"), "---\n---\nDelta\n").unwrap();
        ctx.commit_apply_increments(&scope, &["delta.md".into()], baseline);

        assert!(
            scope
                .take_operator_notes()
                .iter()
                .any(|note| note.contains("abandoned")),
            "generation mismatch must degrade the cache increment"
        );
        let cache = generation_2.test_read_conn();
        assert!(
            !doc_present(&cache, "delta.md"),
            "an N baseline must not publish into generation N+1"
        );
    }

    /// A corruption-class increment failure evicts the generation it ran on. The
    /// request and increment both bind generation 2 (cross-generation increments
    /// are rejected above), so the floor must exclude 2 and the next request must
    /// open generation 3.
    #[test]
    fn increment_corruption_evicts_generation_it_ran_on() {
        let (_tmp, root) = make_seeded_vault();
        let ctx = VaultContext::open_warm(&root).expect("open_warm");

        // Open generation 1, then invalidate it so the mutation request binds 2.
        ctx.begin_request().expect("begin_request generation 1");
        {
            let _cache = ctx.query_cache_unscoped().expect("open generation 1");
        }
        ctx.test_invalidate_current_generation();
        let scope = ctx.begin_request().expect("begin_request generation 2");
        let baseline = ctx
            .query_cache(&scope)
            .expect("reopen generation 2")
            .load_graph_index()
            .expect("generation 2 pre-apply baseline");
        let gen2 = ctx.current_generation().expect("generation 2 current");
        assert_eq!(gen2.number, 2, "the reopen produced generation 2");
        assert_eq!(scope.bound_generation(), 2, "scope bound generation 2");

        // Inject a SQLITE_CORRUPT failure into generation 2's increment commit.
        let corrupt = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            Some("database disk image is malformed".to_string()),
        );
        *gen2.inject_increment_error.lock().unwrap() = Some(CacheError::Sqlite(corrupt));

        // A real changed file so the commit has work to submit; the increment
        // runs on generation 2 and fails with the injected corruption.
        std::fs::write(root.join("delta.md"), "---\ntype: note\n---\nDelta\n").unwrap();
        ctx.commit_apply_increments(&scope, &["delta.md".into()], baseline);

        // The floor must now exclude generation 2: a fresh request reopens to 3.
        ctx.begin_request().expect("begin_request after eviction");
        let _c = ctx
            .query_cache_unscoped()
            .expect("query_cache after eviction");
        assert_eq!(
            gen_number(&ctx),
            3,
            "the eviction must exclude the generation the increment ran on (2)"
        );
    }
}
