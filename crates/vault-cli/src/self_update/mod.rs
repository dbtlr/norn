//! Self-update subcommand: refreshes the running `vault` binary from the
//! latest GitHub release (or a pinned version).

#![allow(dead_code)]

pub mod download;
pub mod manifest;
pub mod receipt;
pub mod render;
pub mod resolve;
pub mod swap;

use serde::Serialize;

use self::resolve::Action;

/// JSON envelope for `vault self-update`. Independent of other report
/// schemas; `schema_version` bumps when this shape changes.
#[derive(Debug, Serialize)]
pub struct SelfUpdateReport {
    pub schema_version: u32,
    pub update_available: bool,
    pub current_version: String,
    pub latest_version: String,
    pub target_version: String,
    pub target_triple: String,
    pub install_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset_sha256: Option<String>,
    pub dry_run: bool,
    pub action: Action,
}

pub const SELF_UPDATE_SCHEMA_VERSION: u32 = 1;

use std::path::PathBuf;

use anyhow::{anyhow, Result};

/// Configuration for a single `vault self-update` invocation.
pub struct RunConfig {
    pub dry_run: bool,
    pub pinned_version: Option<String>,
    /// Override the default install-receipt path. Tests use this; production
    /// passes `receipt::receipt_path()` resolved value.
    pub receipt_path_override: Option<PathBuf>,
    /// Override the running binary path. Tests pass a tempdir; production
    /// passes `std::env::current_exe()` result.
    pub install_path: PathBuf,
    /// URL prefix for the releases endpoint. Tests pass mock server URL;
    /// production passes `https://github.com/dbtlr/vault-cli/releases`.
    pub releases_url: String,
    /// Override compile-time target triple. Tests pass a deterministic value;
    /// production passes `resolve::TARGET_TRIPLE` (None → exit 2).
    pub target_triple: Option<String>,
    /// Current binary version. Production passes `env!("CARGO_PKG_VERSION")`.
    pub current_version: String,
}

pub const BLOCK_MESSAGE: &str = "\
vault self-update only works for installs from the official GitHub install
script. This binary does not have an install receipt.

To update, either:
  • Re-run the installer:
      curl --proto '=https' --tlsv1.2 -LsSf \\
        https://github.com/dbtlr/vault-cli/releases/latest/download/vault-installer.sh | sh

  • Update via the package manager you originally used (cargo, homebrew, etc.)";

pub fn run(cfg: &RunConfig) -> Result<(SelfUpdateReport, i32)> {
    // 1. Receipt check.
    let receipt = match &cfg.receipt_path_override {
        Some(p) => {
            if !p.exists() {
                return Err(anyhow!("BLOCK::no_receipt"));
            }
            let bytes = std::fs::read(p)?;
            serde_json::from_slice::<receipt::Receipt>(&bytes)
                .map_err(|e| anyhow!("parse install receipt: {e}"))?
        }
        None => match receipt::load()? {
            Some(r) => r,
            None => return Err(anyhow!("BLOCK::no_receipt")),
        },
    };

    // 2. Target triple.
    let triple = cfg.target_triple.as_deref().ok_or_else(|| {
        anyhow!(
            "BLOCK::unknown_target: vault was built for a target cargo-dist \
            does not produce a release artifact for (receipt says {})",
            receipt.target
        )
    })?;

    // 3. Resolve manifest URL(s).
    let manifest_url = match &cfg.pinned_version {
        Some(v) => format!("{}/download/v{v}/dist-manifest.json", cfg.releases_url),
        None => format!("{}/latest/download/dist-manifest.json", cfg.releases_url),
    };

    // 4. Fetch manifest(s). In pinned mode, also fetch latest so the
    //    JSON envelope's `latest_version` reflects the true latest.
    let (manifest, latest_version) = if cfg.pinned_version.is_some() {
        let pinned = manifest::fetch(&manifest_url)?;
        let latest_url = format!("{}/latest/download/dist-manifest.json", cfg.releases_url);
        let latest = manifest::fetch(&latest_url)?;
        let latest_v = latest.announcement_version().to_string();
        (pinned, latest_v)
    } else {
        let pinned = manifest::fetch(&manifest_url)?;
        let latest_v = pinned.announcement_version().to_string();
        (pinned, latest_v)
    };

    let target_version = cfg
        .pinned_version
        .clone()
        .unwrap_or_else(|| latest_version.clone());

    // 5. Select asset for this triple.
    let asset = resolve::select_asset(&manifest, triple);
    let action = resolve::determine_action(cfg.dry_run, &target_version, &cfg.current_version);
    let same_version = target_version == cfg.current_version;

    let (asset_url, asset_sha256) = if same_version {
        (None, None)
    } else {
        match asset {
            Some((name, sha)) => (
                Some(format!(
                    "{}/download/v{target_version}/{name}",
                    cfg.releases_url
                )),
                Some(sha.to_string()),
            ),
            None => {
                return Err(anyhow!(
                    "BLOCK::no_asset: release v{target_version} has no artifact for target {triple}"
                ));
            }
        }
    };

    let report = SelfUpdateReport {
        schema_version: SELF_UPDATE_SCHEMA_VERSION,
        update_available: latest_version != cfg.current_version,
        current_version: cfg.current_version.clone(),
        latest_version,
        target_version: target_version.clone(),
        target_triple: triple.to_string(),
        install_path: cfg.install_path.display().to_string(),
        asset_url: asset_url.clone(),
        asset_sha256: asset_sha256.clone(),
        dry_run: cfg.dry_run,
        action,
    };

    // 6. Branch on action.
    match action {
        resolve::Action::WouldUpdate | resolve::Action::WouldNoOp | resolve::Action::NoOp => {
            Ok((report, 0))
        }
        resolve::Action::Updated => {
            let url = asset_url.as_ref().expect("Updated path has asset_url");
            let sha = asset_sha256
                .as_ref()
                .expect("Updated path has asset_sha256");
            let archive_temp =
                download::sibling_temp_path(&cfg.install_path, &format!("{target_version}.tar.xz"));
            download::download_to(url, &archive_temp)?;
            download::verify_sha256(&archive_temp, sha)?;
            let new_binary =
                download::sibling_temp_path(&cfg.install_path, &format!("{target_version}.bin"));
            download::extract_binary(&archive_temp, &new_binary)?;
            let _ = std::fs::remove_file(&archive_temp);
            swap::swap(&new_binary, &cfg.install_path)?;
            Ok((report, 0))
        }
    }
}

/// Classify an error as a block (exit 2) vs runtime fail (exit 1).
/// Block errors are surfaced as `BLOCK::<kind>` anyhow messages.
pub fn classify_exit(err: &anyhow::Error) -> i32 {
    let msg = format!("{err:#}");
    if msg.contains("BLOCK::") {
        2
    } else {
        1
    }
}

#[cfg(test)]
mod run_tests {
    use super::*;
    use crate::self_update::resolve::Action;

    fn fixture_manifest_body() -> &'static str {
        r#"{
            "dist_version": "0.32.0",
            "announcement_tag": "v0.33.1",
            "announcement_title": "v0.33.1",
            "artifacts": {
                "vault-aarch64-apple-darwin.tar.xz": {
                    "name": "vault-aarch64-apple-darwin.tar.xz",
                    "kind": "executable-zip",
                    "target_triples": ["aarch64-apple-darwin"],
                    "checksums": { "sha256": "abc123" }
                }
            }
        }"#
    }

    fn write_receipt(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("install-receipt.json");
        std::fs::write(
            &path,
            r#"{
                "binaries": ["vault"],
                "version": "0.32.0",
                "target": "aarch64-apple-darwin"
            }"#,
        )
        .unwrap();
        path
    }

    #[test]
    fn dry_run_reports_would_update_when_versions_differ() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/latest/download/dist-manifest.json")
            .with_status(200)
            .with_body(fixture_manifest_body())
            .create();

        let tmp = tempfile::tempdir().unwrap();
        let receipt_path = write_receipt(tmp.path());
        let install_path = tmp.path().join("vault");
        std::fs::write(&install_path, b"old binary").unwrap();

        let cfg = RunConfig {
            dry_run: true,
            pinned_version: None,
            receipt_path_override: Some(receipt_path),
            install_path,
            releases_url: server.url(),
            target_triple: Some("aarch64-apple-darwin".to_string()),
            current_version: "0.32.0".to_string(),
        };

        let (report, exit) = run(&cfg).unwrap();
        assert_eq!(exit, 0);
        assert_eq!(report.action, Action::WouldUpdate);
        assert_eq!(report.current_version, "0.32.0");
        assert_eq!(report.target_version, "0.33.1");
        assert_eq!(report.latest_version, "0.33.1");
        assert!(report.update_available);
        assert_eq!(report.asset_sha256.as_deref(), Some("abc123"));
    }

    #[test]
    fn dry_run_reports_would_no_op_when_same_version() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/latest/download/dist-manifest.json")
            .with_status(200)
            .with_body(fixture_manifest_body())
            .create();

        let tmp = tempfile::tempdir().unwrap();
        let receipt_path = write_receipt(tmp.path());
        let install_path = tmp.path().join("vault");
        std::fs::write(&install_path, b"current").unwrap();

        let cfg = RunConfig {
            dry_run: true,
            pinned_version: None,
            receipt_path_override: Some(receipt_path),
            install_path,
            releases_url: server.url(),
            target_triple: Some("aarch64-apple-darwin".to_string()),
            current_version: "0.33.1".to_string(),
        };

        let (report, exit) = run(&cfg).unwrap();
        assert_eq!(exit, 0);
        assert_eq!(report.action, Action::WouldNoOp);
        assert!(!report.update_available);
        assert!(report.asset_url.is_none());
        assert!(report.asset_sha256.is_none());
    }

    #[test]
    fn no_receipt_returns_block_error() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = RunConfig {
            dry_run: true,
            pinned_version: None,
            receipt_path_override: Some(tmp.path().join("missing.json")),
            install_path: tmp.path().join("vault"),
            releases_url: "http://unused".to_string(),
            target_triple: Some("aarch64-apple-darwin".to_string()),
            current_version: "0.32.0".to_string(),
        };
        let err = run(&cfg).unwrap_err();
        assert_eq!(classify_exit(&err), 2);
        let msg = format!("{err:#}");
        assert!(msg.contains("no_receipt"), "got: {msg}");
    }

    #[test]
    fn no_asset_for_triple_returns_block_error() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/latest/download/dist-manifest.json")
            .with_status(200)
            .with_body(fixture_manifest_body())
            .create();

        let tmp = tempfile::tempdir().unwrap();
        let receipt_path = write_receipt(tmp.path());

        let cfg = RunConfig {
            dry_run: true,
            pinned_version: None,
            receipt_path_override: Some(receipt_path),
            install_path: tmp.path().join("vault"),
            releases_url: server.url(),
            target_triple: Some("x86_64-unknown-linux-musl".to_string()),
            current_version: "0.32.0".to_string(),
        };
        let err = run(&cfg).unwrap_err();
        assert_eq!(classify_exit(&err), 2);
        let msg = format!("{err:#}");
        assert!(msg.contains("no_asset"), "got: {msg}");
    }

    #[test]
    fn pinned_version_routes_to_tagged_manifest_url() {
        let mut server = mockito::Server::new();
        // Pinned manifest at the tagged URL
        let _m1 = server
            .mock("GET", "/download/v0.30.0/dist-manifest.json")
            .with_status(200)
            .with_body(
                r#"{
                "dist_version": "0.30.0",
                "announcement_tag": "v0.30.0",
                "announcement_title": "v0.30.0",
                "artifacts": {
                    "vault-aarch64-apple-darwin.tar.xz": {
                        "name": "vault-aarch64-apple-darwin.tar.xz",
                        "kind": "executable-zip",
                        "target_triples": ["aarch64-apple-darwin"],
                        "checksums": { "sha256": "xyz" }
                    }
                }
            }"#,
            )
            .create();
        // True-latest manifest at /latest/ — for `latest_version` field
        let _m2 = server
            .mock("GET", "/latest/download/dist-manifest.json")
            .with_status(200)
            .with_body(fixture_manifest_body()) // returns v0.33.1
            .create();

        let tmp = tempfile::tempdir().unwrap();
        let receipt_path = write_receipt(tmp.path());

        let cfg = RunConfig {
            dry_run: true,
            pinned_version: Some("0.30.0".to_string()),
            receipt_path_override: Some(receipt_path),
            install_path: tmp.path().join("vault"),
            releases_url: server.url(),
            target_triple: Some("aarch64-apple-darwin".to_string()),
            current_version: "0.32.0".to_string(),
        };

        let (report, exit) = run(&cfg).unwrap();
        assert_eq!(exit, 0);
        assert_eq!(report.target_version, "0.30.0");
        assert_eq!(report.latest_version, "0.33.1"); // true latest, not the pinned
        assert!(report.update_available); // latest != current
        assert!(report.asset_sha256.is_some());
    }
}
