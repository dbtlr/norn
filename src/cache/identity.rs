//! Canonical-path-hash identity for the cache directory.

use camino::{Utf8Path, Utf8PathBuf};
use sha2::{Digest, Sha256};

use crate::cache::error::CacheError;

/// Resolves the vault root to its canonical form (symlinks resolved) and
/// returns a stable SHA-256 hex digest of the canonical path.
pub fn vault_identity(vault_root: &Utf8Path) -> Result<(Utf8PathBuf, String), CacheError> {
    let canonical = std::fs::canonicalize(vault_root.as_std_path()).map_err(|e| {
        CacheError::CannotCanonicalize {
            path: vault_root.to_owned(),
            source: e,
        }
    })?;
    let canonical = Utf8PathBuf::from_path_buf(canonical).map_err(|p| CacheError::Io {
        path: vault_root.to_owned(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("canonical path is not valid UTF-8: {}", p.display()),
        ),
    })?;
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_str().as_bytes());
    let hash = hex_lower(hasher.finalize().as_ref());
    Ok((canonical, hash))
}

/// Lowercase hex encoding of a byte slice. Matches the format previously
/// emitted by `format!("{:x}", GenericArray<u8, …>)` on sha2 ≤ 0.10 — the
/// digest type lost its `LowerHex` impl in sha2 0.11, so we encode bytes
/// explicitly. Output is byte-identical to the old formatter for the same
/// input, which is load-bearing: the cache directory name is derived from
/// this hash, so a format change would orphan every existing cache.
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Returns the cache directory path for a given vault root.
/// Format: `<XDG_CACHE_HOME>/vault/<sha256-of-canonical-root>/`, defaulting
/// to `~/.cache/vault/<hash>/` when `XDG_CACHE_HOME` is unset.
pub fn cache_dir_for(vault_root: &Utf8Path) -> Result<(Utf8PathBuf, Utf8PathBuf), CacheError> {
    let (canonical, hash) = vault_identity(vault_root)?;
    let base = xdg_cache_home()?;
    let dir = base.join("vault").join(hash);
    Ok((canonical, dir))
}

fn xdg_state_home() -> Result<Utf8PathBuf, CacheError> {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        if !xdg.is_empty() {
            return Ok(Utf8PathBuf::from(xdg));
        }
    }
    let home = std::env::var("HOME").map_err(|_| CacheError::Io {
        path: Utf8PathBuf::from("$HOME"),
        source: std::io::Error::new(std::io::ErrorKind::NotFound, "HOME not set"),
    })?;
    Ok(Utf8PathBuf::from(home).join(".local").join("state"))
}

/// Returns the state directory path for a given vault root.
/// Format: `<XDG_STATE_HOME>/norn/<sha256-of-canonical-root>/`,
/// defaulting to `~/.local/state/norn/<hash>/` when `XDG_STATE_HOME` is unset.
///
/// Parallel to `cache_dir_for` but uses the state dir (persists across cache
/// clears) and the `norn/` app folder (post-rename; independent of the cache's
/// legacy `vault/` folder).
pub fn state_dir_for(vault_root: &Utf8Path) -> Result<(Utf8PathBuf, Utf8PathBuf), CacheError> {
    let (canonical, hash) = vault_identity(vault_root)?;
    let base = xdg_state_home()?;
    let dir = base.join("norn").join(hash);
    Ok((canonical, dir))
}

/// Events directory for a vault: `<state_dir>/events/`.
///
/// Wired into the telemetry sink; callers resolve the events dir via
/// `crate::cache::events_dir_for`.
pub fn events_dir_for(vault_root: &Utf8Path) -> Result<(Utf8PathBuf, Utf8PathBuf), CacheError> {
    let (canonical, state) = state_dir_for(vault_root)?;
    Ok((canonical, state.join("events")))
}

fn xdg_cache_home() -> Result<Utf8PathBuf, CacheError> {
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        if !xdg.is_empty() {
            return Ok(Utf8PathBuf::from(xdg));
        }
    }
    let home = std::env::var("HOME").map_err(|_| CacheError::Io {
        path: Utf8PathBuf::from("$HOME"),
        source: std::io::Error::new(std::io::ErrorKind::NotFound, "HOME not set"),
    })?;
    Ok(Utf8PathBuf::from(home).join(".cache"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Pins the cache-identity hash format. Two guarantees we need to hold
    /// across sha2 major bumps: the output is lowercase no-separator hex,
    /// and the bytes are identical for the same input. A regression here
    /// would orphan every user's cache directory.
    #[test]
    fn hex_lower_matches_reference_sha256() {
        let mut hasher = Sha256::new();
        hasher.update(b"vault-cli-test-input");
        let hash = hex_lower(hasher.finalize().as_ref());
        assert_eq!(
            hash,
            "950f0173de000add567cf53b9ccb4806f8750a7c33113b5e61109c0ca7a7dc11"
        );
        assert_eq!(hash.len(), 64);
        assert!(hash
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn state_dir_for_format() {
        // state_dir_for should produce a path ending in norn/<hash> under
        // XDG_STATE_HOME (or ~/.local/state/norn/<hash> as fallback).
        // We use a tempdir as the vault root to get a stable canonical path.
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let (_, dir) = state_dir_for(&root).unwrap();
        assert!(
            dir.as_str().contains("/norn/"),
            "path should contain /norn/: {dir}"
        );
        // Hash component is 64-char lowercase hex.
        let hash = dir.file_name().unwrap();
        assert_eq!(hash.len(), 64);
        assert!(hash
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn events_dir_is_under_state_dir() {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let (_, dir) = events_dir_for(&root).unwrap();
        assert!(dir.as_str().contains("/norn/"));
        assert!(dir.as_str().ends_with("/events"));
    }

    #[test]
    fn state_dir_for_uses_xdg_state_home() {
        let tmp = TempDir::new().unwrap();
        let xdg_tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let xdg_str = xdg_tmp.path().to_str().unwrap().to_string();
        std::env::set_var("XDG_STATE_HOME", &xdg_str);
        let result = state_dir_for(&root);
        std::env::remove_var("XDG_STATE_HOME"); // always remove before assert
        let (_, dir) = result.unwrap();
        assert!(
            dir.as_str().starts_with(&xdg_str),
            "should be under XDG_STATE_HOME: {dir}"
        );
    }
}
