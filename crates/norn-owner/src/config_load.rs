//! Vault-config load for the cache warm-up (ADR 0017 resolver-derived config).
//!
//! The owner is the IO-owning, cache-building process, so it is the natural home
//! for the file read that turns a vault's `.norn/config.yaml` into the four cache
//! knobs ([`CacheOpenConfig`]). norn-core stays IO-free: it only parses the
//! injected bytes and derives the index set. This module supplies the bytes.
//!
//! Resolution mirrors the donor `config_loader::load_config`, minus the dead
//! root-precedence logic (root resolution now lives in `norn-config`): an
//! explicit `[vaults.<name>].config` override path wins; otherwise
//! `<vault_root>/.norn/config.yaml` is used if it exists; otherwise the vault
//! runs under [`CacheOpenConfig::default`] (no alias field, no ignores, empty
//! index set).
//!
//! Both errors below feed the NRN-360 user-error surface (a warm-up config
//! failure becomes an `OwnerFrame::Rejected`, not exit-to-heal), but they carry
//! two distinct message shapes: a present-but-unparseable file yields the
//! oracle-matching `invalid config <path>: <detail>` (from
//! [`norn_core::standards::parse_config`]), while a present-but-unreadable file
//! (a permissions/IO access error) yields `failed to read config <path>: <io>`.
//! The oracle renders both the same way (`eprintln!("{error:#}")`, exit 1), so
//! only the parse-error branch is byte-for-byte prefix-identical to it; the
//! access-error branch is an accepted, rarer edge with its own wording.

use camino::{Utf8Path, Utf8PathBuf};

use norn_core::cache::CacheOpenConfig;
use norn_core::standards::VaultConfig;

/// Resolve the config file path an owner should load: an explicit
/// `[vaults.<name>].config` override wins; else `<vault_root>/.norn/config.yaml`
/// if it exists; else `None` (the vault runs under defaults).
fn config_path(vault_root: &Utf8Path, config_override: Option<&Utf8Path>) -> Option<Utf8PathBuf> {
    match config_override {
        Some(p) => Some(p.to_path_buf()),
        None => {
            let default = vault_root.join(".norn/config.yaml");
            default.exists().then_some(default)
        }
    }
}

/// Load the vault's full parsed [`VaultConfig`], or `None` when the vault runs
/// under no config file. Used by the owner to serve `describe`'s structure view
/// (path rules, inbox, schema) — the cache knobs come from [`load_cache_config`].
/// A present-but-unparseable file is a hard error, consistent with the cache
/// load.
pub fn load_vault_config(
    vault_root: &Utf8Path,
    config_override: Option<&Utf8Path>,
) -> anyhow::Result<Option<VaultConfig>> {
    let Some(path) = config_path(vault_root, config_override) else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(path.as_std_path())
        .map_err(|e| anyhow::anyhow!("failed to read config {path}: {e}"))?;
    let config =
        norn_core::standards::parse_config(&text, &path).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(Some(config))
}

/// Load the vault's config into a [`CacheOpenConfig`].
///
/// `config_override` is the registry-resolved `[vaults.<name>].config` path (an
/// absolute or already-grounded path); `None` falls back to the default
/// `<vault_root>/.norn/config.yaml`. A missing file yields defaults; a present
/// but unparseable file is a hard error (the vault cannot be served under an
/// unknown config).
pub fn load_cache_config(
    vault_root: &Utf8Path,
    config_override: Option<&Utf8Path>,
) -> anyhow::Result<CacheOpenConfig> {
    let path: Option<Utf8PathBuf> = match config_override {
        Some(p) => Some(p.to_path_buf()),
        None => {
            let default = vault_root.join(".norn/config.yaml");
            default.exists().then_some(default)
        }
    };

    let Some(path) = path else {
        return Ok(CacheOpenConfig::default());
    };

    let text = std::fs::read_to_string(path.as_std_path())
        .map_err(|e| anyhow::anyhow!("failed to read config {path}: {e}"))?;
    let config =
        norn_core::standards::parse_config(&text, &path).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(CacheOpenConfig::from_vault_config(&config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn root() -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        (tmp, root)
    }

    #[test]
    fn missing_config_yields_defaults() {
        let (_tmp, root) = root();
        let cfg = load_cache_config(&root, None).unwrap();
        assert_eq!(cfg.alias_field, None);
        assert!(cfg.files_ignore.is_empty());
        assert!(cfg.index_set.is_empty());
    }

    #[test]
    fn default_path_config_is_loaded_and_mapped() {
        let (_tmp, root) = root();
        std::fs::create_dir_all(root.join(".norn").as_std_path()).unwrap();
        std::fs::write(
            root.join(".norn/config.yaml").as_std_path(),
            "links:\n  alias_field: aliases\nfiles:\n  ignore:\n    - \"Archive/**\"\nvalidate:\n  rules:\n    - name: r\n      allowed_values:\n        status:\n          - a\n",
        )
        .unwrap();
        let cfg = load_cache_config(&root, None).unwrap();
        assert_eq!(cfg.alias_field.as_deref(), Some("aliases"));
        assert_eq!(cfg.files_ignore, vec!["Archive/**".to_string()]);
        assert!(cfg.index_set.contains("status"));
    }

    #[test]
    fn override_path_wins_over_default() {
        let (_tmp, root) = root();
        // Default path present but pointed away from — the override is loaded.
        std::fs::create_dir_all(root.join(".norn").as_std_path()).unwrap();
        std::fs::write(
            root.join(".norn/config.yaml").as_std_path(),
            "links:\n  alias_field: default_field\n",
        )
        .unwrap();
        let other = root.join("elsewhere.yaml");
        std::fs::write(
            other.as_std_path(),
            "links:\n  alias_field: override_field\n",
        )
        .unwrap();
        let cfg = load_cache_config(&root, Some(&other)).unwrap();
        assert_eq!(cfg.alias_field.as_deref(), Some("override_field"));
    }

    #[test]
    fn unparseable_config_is_a_hard_error() {
        let (_tmp, root) = root();
        std::fs::create_dir_all(root.join(".norn").as_std_path()).unwrap();
        std::fs::write(root.join(".norn/config.yaml").as_std_path(), "not: [valid").unwrap();
        assert!(load_cache_config(&root, None).is_err());
    }

    /// The config-load error message is the oracle's config-error surface
    /// verbatim: `invalid config <path>: <detail>` (NRN-360). The owner carries
    /// this string out to the client as the rejection message, so its shape is a
    /// contract, not incidental.
    #[test]
    fn invalid_config_error_matches_the_oracle_surface() {
        let (_tmp, root) = root();
        std::fs::create_dir_all(root.join(".norn").as_std_path()).unwrap();
        let config = root.join(".norn/config.yaml");
        // An unknown top-level field — a schema-invalid (but well-formed YAML)
        // config, the same class the oracle renders as `invalid config …`.
        std::fs::write(config.as_std_path(), "bogus: true\n").unwrap();
        let err = load_cache_config(&root, None).expect_err("a bad config is an error");
        let message = err.to_string();
        let expected_prefix = format!("invalid config {config}: ");
        assert!(
            message.starts_with(&expected_prefix),
            "expected `{expected_prefix}…`, got {message:?}"
        );
        assert!(
            message.contains("unknown field `bogus`"),
            "expected the serde detail, got {message:?}"
        );
    }
}
