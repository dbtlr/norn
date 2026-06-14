//! Warm vault context for the MCP server.
//!
//! # Design: stateful config, per-call freshness
//!
//! `VaultContext` holds `LoadedConfig` warm across the server's lifetime — config
//! is read once at startup and never re-read until the server restarts. This is
//! intentional: config changes require a server restart, exactly like how the CLI
//! reloads config fresh on every invocation.
//!
//! **The cache is deliberately NOT held as a field.** Each tool call opens a fresh
//! `Cache` handle via `query_cache()`, which calls `open_for_query(...,
//! no_cache_refresh = false)`. This runs the per-invocation incremental freshness
//! check inside `open_for_query`, matching the CLI's per-invocation behavior
//! exactly — vault edits between MCP tool calls are reflected in the next call's
//! results without any filesystem-watcher complexity.
//!
//! The choice is intentional and versioned: v1 of the MCP server is
//! "stateful, warm config, per-call cache freshness, no filesystem watcher".
//! A future version could hold the cache open and use a file-system watcher to
//! push invalidations; that is an explicit scope-extension, not an omission.

use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};

use crate::cache::Cache;
use crate::cache_cmd::open_for_query;
use crate::config_loader::{load_config, LoadedConfig};

/// Warm server-lifetime context for the MCP server.
///
/// Holds a parsed `LoadedConfig` for the vault's lifetime so tool calls do not
/// pay the YAML-parse cost on every invocation. The cache handle is re-opened
/// per tool call to capture vault edits without a filesystem watcher (see module
/// docs for the full design rationale).
pub(crate) struct VaultContext {
    /// Absolute path to the vault root, as passed to `run()` via `--cwd`.
    pub(crate) vault_root: Utf8PathBuf,
    /// Parsed and compiled config, held warm for the server's lifetime.
    pub(crate) config: LoadedConfig,
}

impl VaultContext {
    /// Open the vault context. Reads and parses the config (once). Fails fast
    /// if the config file exists but is unreadable or malformed.
    ///
    /// A missing config file is not an error — `load_config` returns
    /// `LoadedConfig::default()` when no `.norn/config.yaml` is found, so the
    /// server starts cleanly against unconfigured vaults.
    pub(crate) fn open(cwd: &Utf8Path, config_path: Option<&Utf8PathBuf>) -> Result<Self> {
        let config = load_config(&cwd.to_path_buf(), config_path)?;
        Ok(Self {
            vault_root: cwd.to_path_buf(),
            config,
        })
    }

    /// Open a query cache. Runs the per-invocation incremental freshness check
    /// (same as the CLI's implicit refresh on every command).
    ///
    /// Callers should open a cache at the start of each tool call and drop it
    /// when done — do not cache the returned `Cache` handle across calls.
    pub(crate) fn query_cache(&self) -> Result<Cache> {
        open_for_query(
            &self.vault_root,
            self.config.index_options.alias_field.as_deref(),
            false,
        )
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
            ctx.config.index_options.alias_field.is_none(),
            "default config should have no alias_field, got {:?}",
            ctx.config.index_options.alias_field
        );
    }

    #[test]
    fn open_with_config_propagates_alias_field() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-ctx-alias-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let config_dir = root.join(".norn");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.yaml"),
            "links:\n  alias_field: aliases\n",
        )
        .unwrap();

        let ctx = VaultContext::open(&root, None).expect("open with config should succeed");
        assert_eq!(
            ctx.config.index_options.alias_field.as_deref(),
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
        let count: i64 = cache
            .conn()
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .expect("SELECT COUNT(*) FROM documents should succeed");

        assert_eq!(
            count, 3,
            "cache should contain exactly 3 seeded documents, got {count}"
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
            let count: i64 = cache
                .conn()
                .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 3, "initial count should be 3");
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
            let count: i64 = cache
                .conn()
                .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
                .unwrap();
            assert_eq!(
                count, 4,
                "per-call freshness check must index the new document (count should be 4, got {count})"
            );
        }
    }
}
