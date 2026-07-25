//! Owner socket addressing (ADR 0017 + ADR 0012 amendment).
//!
//! A summoned owner's control socket lives in a short runtime dir — NEVER under
//! a per-vault cache path — and its filename is fixed-width: `<h>.<fp>.sock`,
//! where `<h>` is the first 16 hex chars of the blake3 of the **vault identity**
//! (the canonical vault root + the vault config's content identity) and `<fp>`
//! is the build fingerprint (short form).
//!
//! ## Why this shape (decided, NRN-345)
//!
//! - **Fixed-width names in a short base dir structurally eliminate the
//!   `sockaddr_un` `SUN_LEN` overflow class.** A `sun_path` is ~104 bytes;
//!   deriving sockets under long per-vault cache paths risks overflow, which a
//!   silent fallback would mask (a real bug). Hashing the identity to 16 hex
//!   chars gives a bounded name regardless of how long or unicode-heavy the real
//!   root path is; a short runtime base keeps the whole path well under the cap.
//! - **The fingerprint isolates builds.** It keys the socket by ADR 0012 build
//!   identity, so a dev/test binary structurally cannot touch another build's
//!   owner (or any future resident tier). Under ADR 0017 every dev build is
//!   ephemeral; N worktree agents each summon their own owner on their own
//!   socket, and a rebuild mints a new fingerprint → new socket, orphaning the
//!   old owner to idle out.
//! - **The config identity isolates schema revisions.** An owner reads
//!   `.norn/config.yaml` ONCE, at warm-up, and serves every later request under
//!   that in-memory schema — so a socket keyed by the root alone would keep
//!   answering `find` / `get` / `validate` / `set` / `new` under the PREVIOUS
//!   config for the rest of the owner's idle TTL, silently skipping enforcement
//!   the edited config declares. Folding the config's content hash into `<h>`
//!   makes a created, edited, or removed config mint a new socket, so the very
//!   next invocation summons an owner that reads the config from disk. The old
//!   owner is orphaned to idle out — the same trade the build fingerprint
//!   already makes on a rebuild.
//!
//! ## Fingerprint definition (stand-in, flagged)
//!
//! ADR 0012 defines the fingerprint as a blake3 over the sorted `src/` tree +
//! `Cargo.lock`, emitted by a build script. No such build script exists yet,
//! so this uses a **runtime executable-identity** fingerprint —
//! blake3 over `current_exe()`'s path + size + mtime — which satisfies the
//! load-bearing property (different builds → different fingerprints; the client
//! and the owner it spawns hash the same file → the same fingerprint) at O(1)
//! cost. It diverges from ADR 0012 only in that a no-op relink mints a new
//! fingerprint, which the 2026-07-17 amendment (socket-as-address, rebuild →
//! new socket, old owner idles out) explicitly accepts for the ephemeral tier.

use std::path::{Path, PathBuf};

use crate::error::ClientError;

/// Length of the vault-identity hash prefix in the socket name (hex chars).
const ROOT_HASH_HEX_LEN: usize = 16;

/// The config-identity stand-in for a vault the owner would run under DEFAULTS:
/// no override registered and no `<root>/.norn/config.yaml` on disk. Distinct
/// from any content hash, so creating or removing the file mints a different
/// socket.
const NO_CONFIG_IDENTITY: &str = "no-config";

/// Prefix of the config identity for a config the owner would fail to READ — a
/// permissions denial, a directory where a file belongs, or an override path
/// pointing at nothing. Such a config is PRESENT-but-unusable, which the owner
/// reports as `failed to read config <path>: <io>` and exits 1 on; it must
/// therefore never share an identity with [`NO_CONFIG_IDENTITY`] (which serves
/// exit 0 under defaults). The suffix is the `io::ErrorKind` label, so the three
/// unreadable shapes also stay distinct from each other. The label only has to
/// differ — if a toolchain renames a kind, the effect is a new socket and one
/// fresh owner, never a wrong answer.
const UNREADABLE_CONFIG_PREFIX: &str = "unreadable-config:";

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
/// temp dir when `TMPDIR` is unset or empty). This dir is created 0700 at first
/// summon.
///
/// Env-var semantics follow the POSIX-by-default rule (ADR 0020): an *empty*
/// value is treated as unset for both `XDG_RUNTIME_DIR` and `TMPDIR`. An empty
/// `TMPDIR=` therefore falls through to the system temp dir rather than being
/// grounded to a bare relative path (which would fail).
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
        .filter(|value| !value.is_empty())
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

/// The identity of the config an owner summoned for `vault_root` would warm
/// under: the blake3 of the file's bytes when it reads, [`NO_CONFIG_IDENTITY`]
/// when the owner would run under defaults, and an
/// [`UNREADABLE_CONFIG_PREFIX`]-tagged kind when the file is present but the
/// owner's read would fail.
///
/// Resolution mirrors the owner's (`norn-owner`'s `config_path` +
/// `load_cache_config`): an explicit `[vaults.<name>].config` override wins,
/// else `<vault_root>/.norn/config.yaml`. Two asymmetries in that mirror are
/// load-bearing:
///
/// - **A missing DEFAULT path means defaults** — the owner serves the vault with
///   an empty config and exits 0 — so it maps to [`NO_CONFIG_IDENTITY`].
/// - **A missing OVERRIDE path is an ERROR** — the owner reports `failed to read
///   config <path>` and exits 1 — so it maps to the unreadable identity, never
///   to [`NO_CONFIG_IDENTITY`]. Collapsing the two would let a warm
///   defaults-owner keep answering exit 0 where a cold owner exits 1, which is
///   the same class of staleness the config keying exists to close.
///
/// Content-hashed rather than stat-keyed: a same-bytes rewrite (a checkout, a
/// `touch`) must NOT orphan a warm owner, and a same-size edit within one mtime
/// tick must not be missed. The whole file is read on every invocation, so an
/// unbounded config costs its own size per command.
pub fn config_identity(vault_root: &Path, config_override: Option<&Path>) -> String {
    let (path, is_override) = match config_override {
        Some(p) => (p.to_path_buf(), true),
        None => (vault_root.join(".norn").join("config.yaml"), false),
    };
    match std::fs::read(&path) {
        Ok(bytes) => blake3::hash(&bytes).to_hex().to_string(),
        // Absent DEFAULT path only: the owner runs under defaults here. An
        // absent OVERRIDE path falls through to the unreadable arm.
        Err(e) if !is_override && e.kind() == std::io::ErrorKind::NotFound => {
            NO_CONFIG_IDENTITY.to_string()
        }
        Err(e) => format!("{UNREADABLE_CONFIG_PREFIX}{:?}", e.kind()),
    }
}

/// The control socket path for `vault_root` under `runtime_dir` for `fingerprint`
/// and `config_identity`: `<runtime_dir>/<h>.<fp>.sock`. `<h>` is the blake3 of
/// the canonicalized root (best-effort — a not-yet-existing root hashes by its
/// grounded form) joined with the config identity from [`config_identity`].
pub fn socket_path(
    vault_root: &Path,
    runtime_dir: &Path,
    fingerprint: &str,
    config_identity: &str,
) -> PathBuf {
    let canonical = vault_root
        .canonicalize()
        .unwrap_or_else(|_| vault_root.to_path_buf());
    let mut hasher = blake3::Hasher::new();
    hasher.update(canonical.as_os_str().as_encoded_bytes());
    // A separator no path byte can supply, so no root+config pair can collide
    // with a different pair by concatenation.
    hasher.update(&[0u8]);
    hasher.update(config_identity.as_bytes());
    let h = short_hex_n(hasher.finalize().to_hex().as_str(), ROOT_HASH_HEX_LEN);
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
        let sp = socket_path(&long_root, &rt, "deadbeefcafef00d", NO_CONFIG_IDENTITY);
        let name = sp.file_name().unwrap().to_string_lossy();
        // <16 hex>.<16 hex>.sock == 16 + 1 + 16 + 5 == 38 chars, always.
        assert_eq!(name.len(), 38, "socket name must be fixed-width: {name}");
        assert!(name.ends_with(".sock"));
        assert!(sp.starts_with(&rt));
    }

    #[test]
    fn distinct_roots_get_distinct_sockets() {
        let rt = PathBuf::from("/run/norn");
        let a = socket_path(&PathBuf::from("/vault/a"), &rt, "fp00", NO_CONFIG_IDENTITY);
        let b = socket_path(&PathBuf::from("/vault/b"), &rt, "fp00", NO_CONFIG_IDENTITY);
        assert_ne!(a, b);
    }

    #[test]
    fn distinct_fingerprints_get_distinct_sockets() {
        let rt = PathBuf::from("/run/norn");
        let a = socket_path(
            &PathBuf::from("/vault"),
            &rt,
            "aaaaaaaaaaaaaaaa",
            NO_CONFIG_IDENTITY,
        );
        let b = socket_path(
            &PathBuf::from("/vault"),
            &rt,
            "bbbbbbbbbbbbbbbb",
            NO_CONFIG_IDENTITY,
        );
        assert_ne!(a, b, "the fingerprint must isolate builds");
    }

    /// The socket is keyed by the config an owner would warm under: an edited
    /// config addresses a DIFFERENT owner, so the next invocation summons one
    /// that reads the new schema instead of reusing the warm owner's stale one.
    #[test]
    fn distinct_config_identities_get_distinct_sockets() {
        let rt = PathBuf::from("/run/norn");
        let root = PathBuf::from("/vault");
        let before = socket_path(&root, &rt, "fp00", "aaaa");
        let after = socket_path(&root, &rt, "fp00", "bbbb");
        assert_ne!(after, before, "an edited config must mint a new socket");
        let absent = socket_path(&root, &rt, "fp00", NO_CONFIG_IDENTITY);
        assert_ne!(
            absent, before,
            "creating a config must mint a new socket too"
        );
        // Same identity, same socket — an unchanged config reuses the warm owner.
        assert_eq!(before, socket_path(&root, &rt, "fp00", "aaaa"));
    }

    #[test]
    fn config_identity_tracks_content_not_stat() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let absent = config_identity(&root, None);
        assert_eq!(absent, NO_CONFIG_IDENTITY);

        let norn_dir = root.join(".norn");
        std::fs::create_dir_all(&norn_dir).unwrap();
        let config = norn_dir.join("config.yaml");
        std::fs::write(&config, "validate:\n  rules: []\n").unwrap();
        let first = config_identity(&root, None);
        assert_ne!(first, absent, "a created config changes the identity");

        // A same-bytes rewrite must NOT change the identity (no owner churn on a
        // checkout or a `touch`).
        std::fs::write(&config, "validate:\n  rules: []\n").unwrap();
        assert_eq!(config_identity(&root, None), first);

        // An edit does.
        std::fs::write(&config, "validate:\n  rules: []\nfiles:\n  ignore: []\n").unwrap();
        assert_ne!(config_identity(&root, None), first);
    }

    /// The override path is what a registered vault's owner loads, so it is what
    /// the identity must follow — not the (possibly absent) default path.
    #[test]
    fn config_identity_follows_the_override_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let elsewhere = tmp.path().join("elsewhere.yaml");
        std::fs::write(&elsewhere, "files:\n  ignore: []\n").unwrap();
        let with_override = config_identity(&root, Some(&elsewhere));
        assert_ne!(with_override, NO_CONFIG_IDENTITY);
        assert_ne!(
            with_override,
            config_identity(&root, None),
            "the default path is absent here, so the two must differ"
        );
    }

    /// An override pointing at nothing is an owner ERROR (`failed to read config
    /// <path>`, exit 1), not the run-under-defaults case — so it must not share
    /// the no-config identity, or a warm defaults-owner would keep answering
    /// exit 0.
    #[test]
    fn a_missing_override_is_unreadable_not_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let missing = tmp.path().join("nowhere.yaml");
        let identity = config_identity(&root, Some(&missing));
        assert!(
            identity.starts_with(UNREADABLE_CONFIG_PREFIX),
            "expected an unreadable identity, got {identity:?}"
        );
        assert_ne!(identity, config_identity(&root, None));
    }

    /// A present-but-unreadable config (permissions) and a config path that is a
    /// DIRECTORY are both owner read errors, and both must be distinct from the
    /// no-config identity and from a content hash.
    #[cfg(unix)]
    #[test]
    fn present_but_unreadable_configs_are_distinct_from_absent() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("perm");
        let norn_dir = root.join(".norn");
        std::fs::create_dir_all(&norn_dir).unwrap();
        let absent = config_identity(&tmp.path().join("empty"), None);
        assert_eq!(absent, NO_CONFIG_IDENTITY);

        let config = norn_dir.join("config.yaml");
        std::fs::write(&config, "validate:\n  rules: []\n").unwrap();
        let readable = config_identity(&root, None);
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable = config_identity(&root, None);
        // Restore before the tempdir cleanup walks it.
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            unreadable.starts_with(UNREADABLE_CONFIG_PREFIX),
            "a chmod-000 config must read as unreadable, got {unreadable:?}"
        );
        assert_ne!(unreadable, absent);
        assert_ne!(unreadable, readable);

        // A directory where the config file belongs.
        let dir_root = tmp.path().join("asdir");
        std::fs::create_dir_all(dir_root.join(".norn").join("config.yaml")).unwrap();
        let as_dir = config_identity(&dir_root, None);
        assert!(
            as_dir.starts_with(UNREADABLE_CONFIG_PREFIX),
            "a config-as-directory must read as unreadable, got {as_dir:?}"
        );
        assert_ne!(as_dir, absent);
    }

    #[test]
    fn build_fingerprint_is_stable_within_a_process() {
        assert_eq!(build_fingerprint(), build_fingerprint());
        assert_eq!(build_fingerprint().len(), FINGERPRINT_HEX_LEN);
    }
}
