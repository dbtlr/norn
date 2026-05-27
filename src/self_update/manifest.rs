//! cargo-dist `dist-manifest.json` parsing.
//!
//! Only the fields we use are modeled. `serde` ignores everything else
//! by default.

use std::collections::BTreeMap;
use std::io::Read;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct DistManifest {
    pub announcement_tag: String,
    pub artifacts: BTreeMap<String, Artifact>,
}

impl DistManifest {
    /// Strip a leading `v` from `announcement_tag` (`v0.33.1` -> `0.33.1`).
    pub fn announcement_version(&self) -> &str {
        self.announcement_tag
            .strip_prefix('v')
            .unwrap_or(&self.announcement_tag)
    }
}

#[derive(Debug, Deserialize)]
pub struct Artifact {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub target_triples: Vec<String>,
    #[serde(default)]
    pub checksums: BTreeMap<String, String>,
}

/// Fetch and parse a `dist-manifest.json` from the given URL.
///
/// One retry with 1s backoff on transient errors; surfaces 4xx/5xx as
/// hard errors with the status in the message.
pub fn fetch(url: &str) -> anyhow::Result<DistManifest> {
    let body = fetch_body(url)?;
    serde_json::from_slice::<DistManifest>(&body)
        .map_err(|e| anyhow::anyhow!("parse dist-manifest: {e}"))
}

fn fetch_body(url: &str) -> anyhow::Result<Vec<u8>> {
    let mut last_err: Option<anyhow::Error> = None;
    for _attempt in 0..2 {
        match ureq::get(url).call() {
            Ok(response) => {
                let mut buf = Vec::new();
                response
                    .into_reader()
                    .read_to_end(&mut buf)
                    .map_err(|e| anyhow::anyhow!("read manifest body: {e}"))?;
                return Ok(buf);
            }
            Err(ureq::Error::Status(code, _)) => {
                return Err(anyhow::anyhow!("manifest fetch {url} returned HTTP {code}"));
            }
            Err(ureq::Error::Transport(t)) => {
                last_err = Some(anyhow::anyhow!("manifest fetch transport error: {t}"));
                if _attempt == 0 {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("manifest fetch failed")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_MANIFEST: &str = r#"{
        "dist_version": "0.32.0",
        "announcement_tag": "v0.33.1",
        "announcement_title": "Version 0.33.1",
        "artifacts": {
            "vault-aarch64-apple-darwin.tar.xz": {
                "name": "vault-aarch64-apple-darwin.tar.xz",
                "kind": "executable-zip",
                "target_triples": ["aarch64-apple-darwin"],
                "checksums": {
                    "sha256": "abc123def456"
                }
            },
            "vault-x86_64-apple-darwin.tar.xz": {
                "name": "vault-x86_64-apple-darwin.tar.xz",
                "kind": "executable-zip",
                "target_triples": ["x86_64-apple-darwin"],
                "checksums": {
                    "sha256": "deadbeef"
                }
            },
            "vault-aarch64-apple-darwin.tar.xz.sha256": {
                "name": "vault-aarch64-apple-darwin.tar.xz.sha256",
                "kind": "checksum"
            }
        }
    }"#;

    #[test]
    fn parses_manifest() {
        let m: DistManifest = serde_json::from_str(SAMPLE_MANIFEST).unwrap();
        assert_eq!(m.announcement_tag, "v0.33.1");
        assert_eq!(m.artifacts.len(), 3);
    }

    #[test]
    fn announcement_version_strips_v_prefix() {
        let m: DistManifest = serde_json::from_str(SAMPLE_MANIFEST).unwrap();
        assert_eq!(m.announcement_version(), "0.33.1");
    }

    #[test]
    fn fetch_returns_parsed_manifest() {
        let mut server = mockito::Server::new();
        let url = format!("{}/dist-manifest.json", server.url());
        let _m = server
            .mock("GET", "/dist-manifest.json")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(SAMPLE_MANIFEST)
            .create();

        let manifest = fetch(&url).unwrap();
        assert_eq!(manifest.announcement_tag, "v0.33.1");
    }

    #[test]
    fn fetch_returns_err_on_404() {
        let mut server = mockito::Server::new();
        let url = format!("{}/missing.json", server.url());
        let _m = server
            .mock("GET", "/missing.json")
            .with_status(404)
            .create();

        let err = fetch(&url).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("404"), "expected 404 in error, got: {msg}");
    }

    #[test]
    fn fetch_returns_err_on_malformed_body() {
        let mut server = mockito::Server::new();
        let url = format!("{}/bad.json", server.url());
        let _m = server
            .mock("GET", "/bad.json")
            .with_status(200)
            .with_body("{ not json")
            .create();

        let err = fetch(&url).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("parse"), "expected parse error, got: {msg}");
    }
}
