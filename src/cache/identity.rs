//! Canonical-path-hash identity for the cache directory.

use camino::{Utf8Path, Utf8PathBuf};
use sha2::{Digest, Sha256};

use crate::cache::channel::{channel, Channel};
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
    let hash = canonical_vault_identity_hash(&canonical);
    Ok((canonical, hash))
}

/// Hash an already-canonical vault root using the same identity mapping as
/// [`vault_identity`], without touching the filesystem. The service control
/// path uses this for a client-canonicalized `service status --vault` ping so
/// reporting state never performs a blocking canonicalize on the daemon's
/// async accept path.
pub(crate) fn canonical_vault_identity_hash(canonical: &Utf8Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_str().as_bytes());
    hex_lower(hasher.finalize().as_ref())
}

/// Best-effort identity hash for a vault root; None when the root cannot
/// be canonicalized (deleted, permission-denied). Used for prune exemption.
pub(crate) fn vault_identity_hash(vault_root: &Utf8Path) -> Option<String> {
    vault_identity(vault_root).ok().map(|(_, h)| h)
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

/// The on-disk directory segment naming this binary's cache schema version:
/// `v{SCHEMA_VERSION}` (e.g. `v5`). The cache database lives under this segment
/// of its channel dir, so a binary only ever opens its own channel+schema path
/// and cross-version comparison is impossible by construction (NRN-286). Always
/// derived from the constant — never hardcode the literal.
pub(crate) fn schema_segment() -> String {
    format!("v{}", crate::cache::SCHEMA_VERSION)
}

/// Resolved on-disk layout for a vault's cache, split by channel then schema
/// version.
///
/// The `entry_dir` (`<cache_home>/norn/<hash>`) is channel- and
/// schema-independent: the write lock (`.lock`) and shared vault-level state
/// live there, so a dev and a live binary (of any schema) mutating the same
/// vault still serialize against each other. Only the database moves — `db_dir`
/// is `<entry>/v{schema}` for the live channel and `<entry>/dev/v{schema}` for
/// the dev channel (NRN-269 gave the channel split; NRN-286 added the schema
/// segment).
pub(crate) struct CacheLayout {
    pub(crate) canonical: Utf8PathBuf,
    /// `<cache_home>/norn/<hash>` — write lock + shared state, all channels and
    /// schema versions.
    pub(crate) entry_dir: Utf8PathBuf,
    /// Directory holding `cache.db` for the resolved channel + schema version.
    pub(crate) db_dir: Utf8PathBuf,
    /// The channel this layout was resolved for — stored rather than re-derived
    /// from path geometry, so a label can never silently drift from the layout.
    pub(crate) channel: Channel,
}

/// Resolve the full cache layout for a vault under the process channel.
pub(crate) fn cache_layout_for(vault_root: &Utf8Path) -> Result<CacheLayout, CacheError> {
    let base = xdg_cache_home()?;
    cache_layout_in(&base, vault_root)
}

/// [`cache_layout_for`] with the cache-home base passed explicitly. The channel
/// is still resolved from the process (it is a property of the running binary,
/// not of the cache home), so an explicit-home test harness observes the same
/// channel split production does.
pub(crate) fn cache_layout_in(
    cache_home: &Utf8Path,
    vault_root: &Utf8Path,
) -> Result<CacheLayout, CacheError> {
    cache_layout_in_channel(cache_home, vault_root, channel()?)
}

/// [`cache_layout_in`] with the channel supplied explicitly, bypassing the
/// once-per-process resolution. Lets in-process tests exercise both channels
/// against a private cache home without fighting the global `OnceLock`.
pub(crate) fn cache_layout_in_channel(
    cache_home: &Utf8Path,
    vault_root: &Utf8Path,
    channel: Channel,
) -> Result<CacheLayout, CacheError> {
    let (canonical, hash) = vault_identity(vault_root)?;
    let entry_dir = cache_home.join("norn").join(hash);
    let channel_dir = match channel.db_subdir() {
        Some(sub) => entry_dir.join(sub),
        None => entry_dir.clone(),
    };
    let db_dir = channel_dir.join(schema_segment());
    Ok(CacheLayout {
        canonical,
        entry_dir,
        db_dir,
        channel,
    })
}

/// Returns the vault's canonical root plus the directory holding its `cache.db`
/// for the process channel. Format: `<XDG_CACHE_HOME>/norn/<hash>/v{schema}/` on
/// the live channel, `<XDG_CACHE_HOME>/norn/<hash>/dev/v{schema}/` on the dev
/// channel; defaults to `~/.cache/…` when `XDG_CACHE_HOME` is unset. The write
/// lock does NOT live here — it stays at the entry dir; see [`CacheLayout`].
pub fn cache_dir_for(vault_root: &Utf8Path) -> Result<(Utf8PathBuf, Utf8PathBuf), CacheError> {
    let layout = cache_layout_for(vault_root)?;
    Ok((layout.canonical, layout.db_dir))
}

/// The same identity + channel mapping as [`cache_dir_for`] with the cache-home
/// base passed explicitly instead of read from the environment. Test harnesses
/// that manage a private cache home resolve through this without any process-env
/// mutation.
pub(crate) fn cache_dir_in(
    cache_home: &Utf8Path,
    vault_root: &Utf8Path,
) -> Result<(Utf8PathBuf, Utf8PathBuf), CacheError> {
    let layout = cache_layout_in(cache_home, vault_root)?;
    Ok((layout.canonical, layout.db_dir))
}

/// Root of the global cache tree: `<XDG_CACHE_HOME>/norn/`.
pub(crate) fn cache_tree_root() -> Result<Utf8PathBuf, CacheError> {
    Ok(xdg_cache_home()?.join("norn"))
}

/// Root of the global state tree: `<XDG_STATE_HOME>/norn/`.
pub(crate) fn state_tree_root() -> Result<Utf8PathBuf, CacheError> {
    Ok(xdg_state_home()?.join("norn"))
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

/// The `XDG_CACHE_HOME` override when set and non-empty — empty counts as
/// unset. The ONE place that rule lives: [`xdg_cache_home`] resolves the cache
/// base through it, and `norn service install` bakes the same answer into the
/// daemon's launchd environment, so the plist can never bake an override this
/// derivation wouldn't use.
pub(crate) fn xdg_cache_home_env() -> Option<Utf8PathBuf> {
    match std::env::var("XDG_CACHE_HOME") {
        Ok(xdg) if !xdg.is_empty() => Some(Utf8PathBuf::from(xdg)),
        _ => None,
    }
}

fn xdg_cache_home() -> Result<Utf8PathBuf, CacheError> {
    if let Some(xdg) = xdg_cache_home_env() {
        return Ok(xdg);
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
        hasher.update(b"norn-test-input");
        let hash = hex_lower(hasher.finalize().as_ref());
        assert_eq!(
            hash,
            "6bf80c1353552aed7d974919d3c43a2ed39dacb57ada8019d625cc2efda0c844"
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

    /// Path derivation per channel + schema version: the live db sits under a
    /// `v{schema}` segment of the vault entry dir; the dev db nests under
    /// `dev/v{schema}`; and the write-lock (entry) dir is byte-identical across
    /// both channels and schema versions.
    #[test]
    fn cache_layout_splits_db_but_shares_entry_across_channels() {
        let tmp = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let home = Utf8PathBuf::from_path_buf(home.path().to_path_buf()).unwrap();

        let live = cache_layout_in_channel(&home, &root, Channel::Live).unwrap();
        let dev = cache_layout_in_channel(&home, &root, Channel::Dev).unwrap();

        let schema = schema_segment();
        // Entry dir (lock lives here) is identical for both channels.
        assert_eq!(live.entry_dir, dev.entry_dir);
        // Live db is `<entry>/v{schema}`; dev db is `<entry>/dev/v{schema}`.
        assert_eq!(live.db_dir, live.entry_dir.join(&schema));
        assert_eq!(dev.db_dir, dev.entry_dir.join("dev").join(&schema));
        assert_ne!(live.db_dir, dev.db_dir);
        // The lock/entry dir stays the 64-hex vault hash, unqualified.
        assert_eq!(live.entry_dir.file_name().unwrap().len(), 64);
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
