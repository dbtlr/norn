//! Durable-artifact directory resolution for a REGISTERED vault (NRN-400).
//!
//! Registration is what unlocks durable artifacts (cache / event stream /
//! logs): a registered vault gets a stable per-vault state home; an ephemeral
//! (unregistered) vault keeps everything in memory. This module resolves the
//! events directory the durable mutation-telemetry store writes under — the
//! sole XDG-reading step, kept here (the central-config IO crate) so norn-core
//! stays root-free and receives the dir as a value.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// XDG base for durable state (the event stream / logs live under it). Same
/// precedence rule as the config home: the env var when set to an absolute
/// path, else `$HOME/.local/state`.
pub const XDG_STATE_HOME_ENV: &str = "XDG_STATE_HOME";

/// The events directory a registered vault's durable telemetry store writes
/// under (NRN-400). Returns `None` when no durable location can be resolved
/// (no `logs` override AND no state home) — telemetry then degrades to
/// in-memory rather than failing a mutation, since the audit trail is never
/// worth aborting a write over.
///
/// - `logs_override`: the registry `[vaults.<name>].logs` path, when one was
///   registered — its `events/` subdirectory is used verbatim.
/// - otherwise: `<state_home>/norn/<hash>/events`, where `state_home` is
///   `$XDG_STATE_HOME` (absolute) or `$HOME/.local/state`, and `<hash>` is the
///   blake3 hex of the canonical vault root (stable per vault).
///
/// `getenv` is injected so precedence is unit-testable without touching the
/// process environment.
pub fn events_dir_for(
    getenv: impl Fn(&str) -> Option<OsString>,
    canonical_root: &Path,
    logs_override: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(logs) = logs_override {
        return Some(logs.join("events"));
    }
    let base = state_home(&getenv)?;
    let hash = blake3::hash(canonical_root.as_os_str().as_encoded_bytes());
    let hash = &hash.to_hex()[..32];
    Some(base.join("norn").join(hash).join("events"))
}

/// Resolve the durable state home: `$XDG_STATE_HOME` (absolute, non-empty) or
/// `$HOME/.local/state`. `None` when neither is available.
fn state_home(getenv: &impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    if let Some(xdg) = getenv(XDG_STATE_HOME_ENV).filter(|v| !v.is_empty()) {
        let xdg = PathBuf::from(xdg);
        if xdg.is_absolute() {
            return Some(xdg);
        }
    }
    let home = getenv("HOME")
        .map(PathBuf::from)
        .filter(|v| v.is_absolute())?;
    Some(home.join(".local").join("state"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key: &str| {
            owned
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn logs_override_uses_its_events_subdir_verbatim() {
        let dir = events_dir_for(
            env_from(&[("HOME", "/home/user")]),
            Path::new("/vault"),
            Some(Path::new("/custom/logs")),
        )
        .unwrap();
        assert_eq!(dir, Path::new("/custom/logs/events"));
    }

    #[test]
    fn xdg_state_home_wins_and_is_hashed_per_root() {
        let dir = events_dir_for(
            env_from(&[("XDG_STATE_HOME", "/xdg/state"), ("HOME", "/home/user")]),
            Path::new("/vault"),
            None,
        )
        .unwrap();
        let s = dir.to_str().unwrap();
        assert!(s.starts_with("/xdg/state/norn/"), "under state home: {s}");
        assert!(s.ends_with("/events"));
        // Distinct roots hash to distinct dirs.
        let other = events_dir_for(
            env_from(&[("XDG_STATE_HOME", "/xdg/state")]),
            Path::new("/other-vault"),
            None,
        )
        .unwrap();
        assert_ne!(dir, other);
    }

    #[test]
    fn falls_back_to_home_local_state() {
        let dir = events_dir_for(
            env_from(&[("HOME", "/home/user")]),
            Path::new("/vault"),
            None,
        )
        .unwrap();
        assert!(dir
            .to_str()
            .unwrap()
            .starts_with("/home/user/.local/state/norn/"));
    }

    #[test]
    fn none_when_no_durable_location_resolvable() {
        assert!(events_dir_for(env_from(&[]), Path::new("/vault"), None).is_none());
    }

    #[test]
    fn relative_state_home_is_ignored() {
        // A relative XDG_STATE_HOME is invalid; fall through to HOME.
        let dir = events_dir_for(
            env_from(&[("XDG_STATE_HOME", "relative"), ("HOME", "/home/user")]),
            Path::new("/vault"),
            None,
        )
        .unwrap();
        assert!(dir
            .to_str()
            .unwrap()
            .starts_with("/home/user/.local/state/"));
    }
}
