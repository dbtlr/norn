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

use camino::{Utf8Path, Utf8PathBuf};

use norn_core::cache::CacheOpenConfig;

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
}
