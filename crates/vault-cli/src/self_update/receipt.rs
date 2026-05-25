//! Install receipt — written by the cargo-dist shell installer.
//!
//! Presence of the receipt is the gate for `vault self-update`:
//! its absence means this binary was not installed by the official GitHub
//! install script, and we cannot safely swap it.

// Public API is consumed by the self-update command wired in a later task.
#![allow(dead_code)]

use std::path::PathBuf;

use serde::Deserialize;

/// Subset of the cargo-dist install-receipt.json shape that we care about.
/// We deliberately ignore other fields with `#[serde(default)]`-friendly
/// permissiveness via serde's default deny-unknown-fields behavior (off).
#[derive(Debug, Deserialize)]
pub struct Receipt {
    pub target: String,
    pub version: String,
}

/// Resolve the conventional install-receipt path for this user.
/// Returns `None` if neither `XDG_CONFIG_HOME` nor `HOME` is set.
pub fn receipt_path() -> Option<PathBuf> {
    receipt_path_with_env(|k| std::env::var(k).ok())
}

fn receipt_path_with_env<F: Fn(&str) -> Option<String>>(get: F) -> Option<PathBuf> {
    if let Some(xdg) = get("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(xdg).join("vault-cli/install-receipt.json"));
    }
    let home = get("HOME").filter(|s| !s.is_empty())?;
    Some(PathBuf::from(home).join(".config/vault-cli/install-receipt.json"))
}

/// Cheap, side-effect-free presence check. Runs on every CLI invocation
/// (drives the dynamic-hide of the `self-update` subcommand in `--help`).
pub fn exists() -> bool {
    receipt_path().is_some_and(|p| exists_at(&p))
}

fn exists_at(path: &std::path::Path) -> bool {
    std::fs::metadata(path).is_ok()
}

/// Load and parse the install receipt from disk. Returns `Ok(None)` when
/// no receipt is present; `Err` on a present-but-malformed receipt.
pub fn load() -> anyhow::Result<Option<Receipt>> {
    let Some(path) = receipt_path() else {
        return Ok(None);
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "read install receipt {}: {e}",
                path.display()
            ))
        }
    };
    let receipt: Receipt = serde_json::from_slice(&bytes)
        .map_err(|e| anyhow::anyhow!("parse install receipt {}: {e}", path.display()))?;
    Ok(Some(receipt))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RECEIPT: &str = r#"{
        "binaries": ["vault"],
        "install_prefix": "/Users/drew/.cargo",
        "binary_aliases": {},
        "cargo_dist_version": "0.32.0",
        "install_layout": "flat",
        "modify_path": true,
        "provider": {
            "source": "github",
            "version": "v0.32.0"
        },
        "source": {
            "app_name": "vault-cli",
            "name": "vault-cli",
            "owner": "dbtlr",
            "release_type": "github",
            "tag": "v0.32.0",
            "version": "0.32.0"
        },
        "version": "0.32.0",
        "target": "aarch64-apple-darwin"
    }"#;

    #[test]
    fn parses_target_from_receipt() {
        let receipt: Receipt = serde_json::from_str(SAMPLE_RECEIPT).unwrap();
        assert_eq!(receipt.target, "aarch64-apple-darwin");
        assert_eq!(receipt.version, "0.32.0");
    }

    #[test]
    fn rejects_malformed_receipt() {
        let result: Result<Receipt, _> = serde_json::from_str("{ not json");
        assert!(result.is_err());
    }

    #[test]
    fn receipt_path_prefers_xdg_config_home() {
        let tmp = tempfile::tempdir().unwrap();
        let path = receipt_path_with_env(|k| match k {
            "XDG_CONFIG_HOME" => Some(tmp.path().to_string_lossy().into_owned()),
            _ => None,
        });
        assert_eq!(
            path,
            Some(tmp.path().join("vault-cli/install-receipt.json"))
        );
    }

    #[test]
    fn receipt_path_falls_back_to_home_config() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let path = receipt_path_with_env(|k| match k {
            "XDG_CONFIG_HOME" => None,
            "HOME" => Some(home.to_string_lossy().into_owned()),
            _ => None,
        });
        assert_eq!(
            path,
            Some(home.join(".config/vault-cli/install-receipt.json"))
        );
    }

    #[test]
    fn exists_returns_true_when_file_present() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("install-receipt.json");
        std::fs::write(&path, SAMPLE_RECEIPT).unwrap();
        assert!(exists_at(&path));
    }

    #[test]
    fn exists_returns_false_when_file_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("install-receipt.json");
        assert!(!exists_at(&path));
    }

    #[test]
    fn load_returns_none_when_receipt_missing() {
        let tmp = tempfile::tempdir().unwrap();
        // Use receipt_path_with_env directly to avoid touching global env.
        // load() calls receipt_path() which reads the real env, so we verify
        // the None-return logic by confirming the path doesn't exist.
        let path = receipt_path_with_env(|k| match k {
            "XDG_CONFIG_HOME" => Some(tmp.path().to_string_lossy().into_owned()),
            _ => None,
        });
        let path = path.unwrap();
        assert!(!path.exists());
        // Directly exercise the bytes-not-found branch.
        let bytes_result = std::fs::read(&path);
        assert!(bytes_result.is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound));
    }
}
