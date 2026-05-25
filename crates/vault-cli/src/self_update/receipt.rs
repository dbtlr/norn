//! Install receipt — written by the cargo-dist shell installer.
//!
//! Presence of the receipt is the gate for `vault self-update`:
//! its absence means this binary was not installed by the official GitHub
//! install script, and we cannot safely swap it.

use serde::Deserialize;

/// Subset of the cargo-dist install-receipt.json shape that we care about.
/// We deliberately ignore other fields with `#[serde(default)]`-friendly
/// permissiveness via serde's default deny-unknown-fields behavior (off).
// Task 3+ will add receipt_path() and exists() consumers; suppress dead_code until then.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct Receipt {
    pub target: String,
    pub version: String,
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
}
