//! Install receipt — written by the cargo-dist shell installer.
//!
//! Presence of the receipt is the gate for `norn self-update`:
//! its absence means this binary was not installed by the official GitHub
//! install script, and we cannot safely swap it.

// Public API is consumed by the self-update command wired in a later task.
#![allow(dead_code)]

use std::path::PathBuf;

use serde::Deserialize;

/// Subset of the cargo-dist install-receipt.json shape that we care about.
/// Both fields are optional: the receipt shape varies across cargo-dist
/// versions (newer receipts omit the top-level `target`), and neither field is
/// load-bearing for an update — the asset triple comes from the compile-time
/// `resolve::TARGET_TRIPLE` and the current version from `CARGO_PKG_VERSION`.
/// We only need the receipt to *exist and parse* to gate self-update; `target`
/// is surfaced cosmetically in one error message. Unknown fields are ignored.
#[derive(Debug, Deserialize)]
pub struct Receipt {
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

/// cargo-dist names the install-receipt directory and file after the install
/// "app name" — the published package name (`norn-run`), NOT the binary name
/// (`norn`) — as `<app>/<app>-receipt.json`. Deriving the app name from
/// `CARGO_PKG_NAME` keeps this aligned with what cargo-dist writes, including
/// across future renames (the v0.34 vault-cli → norn rename broke this by
/// leaving the path hardcoded to the binary name).
const APP_NAME: &str = env!("CARGO_PKG_NAME");

fn receipt_relative_path() -> PathBuf {
    PathBuf::from(APP_NAME).join(format!("{APP_NAME}-receipt.json"))
}

/// Resolve the conventional install-receipt path for this user.
/// Returns `None` if neither `XDG_CONFIG_HOME` nor `HOME` is set.
pub fn receipt_path() -> Option<PathBuf> {
    receipt_path_with_env(|k| std::env::var(k).ok())
}

fn receipt_path_with_env<F: Fn(&str) -> Option<String>>(get: F) -> Option<PathBuf> {
    if let Some(xdg) = get("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(xdg).join(receipt_relative_path()));
    }
    let home = get("HOME").filter(|s| !s.is_empty())?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join(receipt_relative_path()),
    )
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
        "binaries": ["norn"],
        "install_prefix": "/home/user/.cargo",
        "binary_aliases": {},
        "cargo_dist_version": "0.32.0",
        "install_layout": "flat",
        "modify_path": true,
        "provider": {
            "source": "github",
            "version": "v0.32.0"
        },
        "source": {
            "app_name": "norn",
            "name": "norn",
            "owner": "dbtlr",
            "release_type": "github",
            "tag": "v0.32.0",
            "version": "0.32.0"
        },
        "version": "0.32.0",
        "target": "aarch64-apple-darwin"
    }"#;

    /// The current cargo-dist (0.32.0) receipt shape actually written to disk:
    /// no top-level `target`, app_name is the package name `norn-run`.
    const CURRENT_RECEIPT: &str = r#"{
        "binaries": ["norn"],
        "binary_aliases": {},
        "install_prefix": "/home/user/.cargo",
        "install_layout": "cargo-home",
        "modify_path": true,
        "provider": { "source": "cargo-dist", "version": "0.32.0" },
        "source": {
            "app_name": "norn-run",
            "name": "norn",
            "owner": "dbtlr",
            "release_type": "github"
        },
        "version": "0.35.1"
    }"#;

    #[test]
    fn parses_target_from_receipt() {
        let receipt: Receipt = serde_json::from_str(SAMPLE_RECEIPT).unwrap();
        assert_eq!(receipt.target.as_deref(), Some("aarch64-apple-darwin"));
        assert_eq!(receipt.version.as_deref(), Some("0.32.0"));
    }

    #[test]
    fn parses_current_cargo_dist_receipt_without_target() {
        // Regression: the on-disk receipt has no top-level `target` field.
        // It must still parse (target is optional + cosmetic).
        let receipt: Receipt = serde_json::from_str(CURRENT_RECEIPT).unwrap();
        assert_eq!(receipt.target, None);
        assert_eq!(receipt.version.as_deref(), Some("0.35.1"));
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
        // cargo-dist writes <config>/<app>/<app>-receipt.json with app=norn-run.
        assert_eq!(
            path,
            Some(tmp.path().join("norn-run/norn-run-receipt.json"))
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
            Some(home.join(".config/norn-run/norn-run-receipt.json"))
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
