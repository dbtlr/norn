//! Download the release tarball, verify sha256, extract the `vault` binary.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{anyhow, Result};
use sha2::{Digest, Sha256};

/// Download `url` into `dest`. Streams the body to disk; does not buffer
/// the whole tarball in memory.
pub fn download_to(url: &str, dest: &Path) -> Result<()> {
    let mut last_err: Option<anyhow::Error> = None;
    for _attempt in 0..2 {
        match ureq::get(url).call() {
            Ok(response) => {
                let mut reader = response.into_reader();
                let mut file = fs::File::create(dest)
                    .map_err(|e| anyhow!("create {}: {e}", dest.display()))?;
                std::io::copy(&mut reader, &mut file)
                    .map_err(|e| anyhow!("stream body to {}: {e}", dest.display()))?;
                file.flush()
                    .map_err(|e| anyhow!("flush {}: {e}", dest.display()))?;
                return Ok(());
            }
            Err(ureq::Error::Status(code, _)) => {
                return Err(anyhow!("download {url}: HTTP {code}"));
            }
            Err(ureq::Error::Transport(t)) => {
                last_err = Some(anyhow!("download transport error: {t}"));
                if _attempt == 0 {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("download failed")))
}

/// Verify the sha256 of `path` matches `expected`. Hex-encoded lowercase.
pub fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let mut file = fs::File::open(path).map_err(|e| anyhow!("open {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| anyhow!("read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let got = hex_lower(&hasher.finalize());
    if got != expected {
        return Err(anyhow!(
            "sha256 mismatch for {}: expected {expected}, got {got}",
            path.display()
        ));
    }
    Ok(())
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_writes_body_to_destination() {
        let mut server = mockito::Server::new();
        let url = format!("{}/vault.tar.xz", server.url());
        let body = b"hello world";
        let _m = server
            .mock("GET", "/vault.tar.xz")
            .with_status(200)
            .with_body(body)
            .create();

        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("vault.tar.xz");
        download_to(&url, &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), body);
    }

    #[test]
    fn verify_sha256_ok_when_match() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("blob");
        fs::write(&file, b"hello world").unwrap();
        let expected = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        verify_sha256(&file, expected).unwrap();
    }

    #[test]
    fn verify_sha256_err_on_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("blob");
        fs::write(&file, b"hello world").unwrap();
        let err = verify_sha256(&file, "deadbeef").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("sha256"), "expected sha256 mention: {msg}");
    }
}
