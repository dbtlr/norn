//! Owner socket addressing (ADR 0017 + ADR 0012 amendment).
//!
//! A summoned owner's control socket lives in a short runtime dir — NEVER under
//! a per-vault cache path — and its filename is fixed-width: `<h>.<fp>.sock`,
//! where `<h>` is the first 16 hex chars of the blake3 of the canonical vault
//! root and `<fp>` is the build fingerprint (short form).
//!
//! ## Why this shape (decided, NRN-345)
//!
//! - **Fixed-width names in a short base dir structurally eliminate the
//!   `sockaddr_un` `SUN_LEN` overflow class.** A `sun_path` is ~104 bytes; the
//!   donor derived sockets under long per-vault cache paths and papered over the
//!   overflow with a silent fallback (a real bug). Hashing the root to 16 hex
//!   chars gives a bounded name regardless of how long or unicode-heavy the real
//!   root path is; a short runtime base keeps the whole path well under the cap.
//! - **The fingerprint isolates builds.** It keys the socket by ADR 0012 build
//!   identity, so a dev/test binary structurally cannot touch another build's
//!   owner (or any future resident tier). Under ADR 0017 every dev build is
//!   ephemeral; N worktree agents each summon their own owner on their own
//!   socket, and a rebuild mints a new fingerprint → new socket, orphaning the
//!   old owner to idle out.
//!
//! ## Fingerprint definition (stand-in, flagged)
//!
//! ADR 0012 defines the fingerprint as a blake3 over the sorted `src/` tree +
//! `Cargo.lock`, emitted by a build script. The rewrite tree has no such build
//! script yet, so this uses a **runtime executable-identity** fingerprint —
//! blake3 over `current_exe()`'s path + size + mtime — which satisfies the
//! load-bearing property (different builds → different fingerprints; the client
//! and the owner it spawns hash the same file → the same fingerprint) at O(1)
//! cost. It diverges from ADR 0012 only in that a no-op relink mints a new
//! fingerprint, which the 2026-07-17 amendment (socket-as-address, rebuild →
//! new socket, old owner idles out) explicitly accepts for the ephemeral tier.

use std::path::{Path, PathBuf};

use crate::error::ClientError;

/// Length of the vault-root hash prefix in the socket name (hex chars).
const ROOT_HASH_HEX_LEN: usize = 16;

/// Length of the build-fingerprint segment in the socket name (hex chars).
const FINGERPRINT_HEX_LEN: usize = 16;

/// The build fingerprint (short form) — see the module docs. Runtime
/// executable-identity: blake3 over `current_exe()`'s path + size + mtime,
/// truncated to [`FINGERPRINT_HEX_LEN`] hex chars. Falls back to a fixed
/// sentinel if the exe cannot be identified (so addressing still functions;
/// isolation degrades to per-host, which is acceptable for that rare case).
pub fn build_fingerprint() -> String {
    match current_exe_identity() {
        Some(bytes) => short_hex_n(blake3::hash(&bytes).to_hex().as_str(), FINGERPRINT_HEX_LEN),
        None => "0".repeat(FINGERPRINT_HEX_LEN),
    }
}

fn current_exe_identity() -> Option<Vec<u8>> {
    let exe = std::env::current_exe().ok()?;
    let meta = std::fs::metadata(&exe).ok()?;
    let mut bytes = exe.as_os_str().as_encoded_bytes().to_vec();
    bytes.extend_from_slice(&meta.len().to_le_bytes());
    if let Ok(mtime) = meta.modified() {
        if let Ok(dur) = mtime.duration_since(std::time::UNIX_EPOCH) {
            bytes.extend_from_slice(&dur.as_nanos().to_le_bytes());
        }
    }
    Some(bytes)
}

/// The runtime dir base for owner sockets, from the environment: `$XDG_RUNTIME_DIR/norn`
/// when set and non-empty, else `$TMPDIR/norn-<uid>` (falling back to the system
/// temp dir when `TMPDIR` is unset). This dir is created 0700 at first summon.
///
/// Env-scoped for hermetic tests: pass a value into [`socket_path`] directly
/// rather than mutating the process environment.
pub fn runtime_dir_from_env() -> Result<PathBuf, ClientError> {
    if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("norn"));
        }
    }
    let base = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    if base.as_os_str().is_empty() {
        return Err(ClientError::NoRuntimeDir);
    }
    Ok(base.join(format!("norn-{}", current_uid())))
}

#[cfg(unix)]
pub(crate) fn current_uid() -> u32 {
    // SAFETY: `getuid` is always-succeeds, no args, no memory effects.
    #[allow(unsafe_code)]
    unsafe {
        libc::getuid()
    }
}

#[cfg(not(unix))]
pub(crate) fn current_uid() -> u32 {
    0
}

/// The control socket path for `vault_root` under `runtime_dir` for `fingerprint`:
/// `<runtime_dir>/<h>.<fp>.sock`. `<h>` is the blake3 of the canonicalized root
/// (best-effort — a not-yet-existing root hashes by its grounded form).
pub fn socket_path(vault_root: &Path, runtime_dir: &Path, fingerprint: &str) -> PathBuf {
    let canonical = vault_root
        .canonicalize()
        .unwrap_or_else(|_| vault_root.to_path_buf());
    let h = short_hex_n(
        blake3::hash(canonical.as_os_str().as_encoded_bytes())
            .to_hex()
            .as_str(),
        ROOT_HASH_HEX_LEN,
    );
    let fp = short_hex_n(fingerprint, FINGERPRINT_HEX_LEN);
    runtime_dir.join(format!("{h}.{fp}.sock"))
}

fn short_hex_n(hex: &str, n: usize) -> String {
    hex.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn socket_name_is_fixed_width_and_bounded() {
        let rt = PathBuf::from("/run/norn");
        let long_root = PathBuf::from("/some/extremely/long/and/deeply/nested/unicode-héavy/παθ/that/would/blow/sun_len/if/used/verbatim/vault");
        let sp = socket_path(&long_root, &rt, "deadbeefcafef00d");
        let name = sp.file_name().unwrap().to_string_lossy();
        // <16 hex>.<16 hex>.sock == 16 + 1 + 16 + 5 == 38 chars, always.
        assert_eq!(name.len(), 38, "socket name must be fixed-width: {name}");
        assert!(name.ends_with(".sock"));
        assert!(sp.starts_with(&rt));
    }

    #[test]
    fn distinct_roots_get_distinct_sockets() {
        let rt = PathBuf::from("/run/norn");
        let a = socket_path(&PathBuf::from("/vault/a"), &rt, "fp00");
        let b = socket_path(&PathBuf::from("/vault/b"), &rt, "fp00");
        assert_ne!(a, b);
    }

    #[test]
    fn distinct_fingerprints_get_distinct_sockets() {
        let rt = PathBuf::from("/run/norn");
        let a = socket_path(&PathBuf::from("/vault"), &rt, "aaaaaaaaaaaaaaaa");
        let b = socket_path(&PathBuf::from("/vault"), &rt, "bbbbbbbbbbbbbbbb");
        assert_ne!(a, b, "the fingerprint must isolate builds");
    }

    #[test]
    fn build_fingerprint_is_stable_within_a_process() {
        assert_eq!(build_fingerprint(), build_fingerprint());
        assert_eq!(build_fingerprint().len(), FINGERPRINT_HEX_LEN);
    }
}
