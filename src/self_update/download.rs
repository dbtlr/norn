//! Download the release tarball, verify sha256, extract the `vault` binary.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

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

/// Extract the `vault` binary out of a cargo-dist `.tar.xz` to `dest`.
/// Sets the resulting file mode to 0o755. Errors if the archive does not
/// contain a file whose basename is `vault`.
pub fn extract_binary(archive: &Path, dest: &Path) -> Result<()> {
    let file =
        fs::File::open(archive).map_err(|e| anyhow!("open archive {}: {e}", archive.display()))?;
    let xz = xz2::read::XzDecoder::new(file);
    let mut tar = tar::Archive::new(xz);
    for entry in tar.entries().map_err(|e| anyhow!("read archive: {e}"))? {
        let mut entry = entry.map_err(|e| anyhow!("read archive entry: {e}"))?;
        let path = entry.path().map_err(|e| anyhow!("read entry path: {e}"))?;
        if path.file_name().and_then(|s| s.to_str()) == Some("vault") {
            let mut out =
                fs::File::create(dest).map_err(|e| anyhow!("create {}: {e}", dest.display()))?;
            std::io::copy(&mut entry, &mut out)
                .map_err(|e| anyhow!("write {}: {e}", dest.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(dest, fs::Permissions::from_mode(0o755))
                    .map_err(|e| anyhow!("chmod {}: {e}", dest.display()))?;
            }
            return Ok(());
        }
    }
    Err(anyhow!(
        "archive {} did not contain a vault binary",
        archive.display()
    ))
}

/// Temp path adjacent to `install_path`, with a `.vault-self-update-*` prefix.
/// Same-filesystem placement is required for atomic rename.
pub fn sibling_temp_path(install_path: &Path, suffix: &str) -> PathBuf {
    let parent = install_path.parent().unwrap_or_else(|| Path::new("."));
    let stem = install_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("vault");
    parent.join(format!(".{stem}-self-update-{suffix}"))
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

    #[test]
    fn extract_binary_pulls_vault_from_tarball() {
        let tmp = tempfile::tempdir().unwrap();
        let archive_path = tmp.path().join("release.tar.xz");

        // Build a tar.xz containing `vault-foo/vault` and a noise file.
        let xz_writer = xz2::write::XzEncoder::new(fs::File::create(&archive_path).unwrap(), 6);
        let mut builder = tar::Builder::new(xz_writer);

        let mut header = tar::Header::new_gnu();
        header.set_path("vault-foo/vault").unwrap();
        header.set_size(11);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append(&header, &b"fake binary"[..]).unwrap();

        let mut header = tar::Header::new_gnu();
        header.set_path("vault-foo/README.md").unwrap();
        header.set_size(5);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, &b"noise"[..]).unwrap();

        builder.into_inner().unwrap().finish().unwrap();

        let dest = tmp.path().join("vault.new");
        extract_binary(&archive_path, &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"fake binary");
    }

    #[test]
    fn extract_binary_errors_when_no_vault_in_tarball() {
        let tmp = tempfile::tempdir().unwrap();
        let archive_path = tmp.path().join("empty.tar.xz");

        let xz_writer = xz2::write::XzEncoder::new(fs::File::create(&archive_path).unwrap(), 6);
        let builder = tar::Builder::new(xz_writer);
        builder.into_inner().unwrap().finish().unwrap();

        let dest = tmp.path().join("vault.new");
        let err = extract_binary(&archive_path, &dest).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("vault"),
            "expected mention of vault binary: {msg}"
        );
    }
}
