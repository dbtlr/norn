//! Cache channel: `live` vs `dev` isolation.
//!
//! A binary running from a cargo build tree (`cargo run`, `cargo test`, a
//! freshly-built `target/…/norn`) must never read, migrate, invalidate, or
//! write the cache namespace the installed (live) binary uses. `Cache::open`'s
//! older-schema silent rebuild otherwise lets a dev binary migrate the live
//! per-vault cache out from under an installed client, locking it out with the
//! upgrade-required error.
//!
//! Resolution order (resolved once per process; see [`channel`]):
//!   1. `NORN_CACHE_CHANNEL` env var — exactly `live` or `dev`; any other
//!      value (including empty) is a hard error, since an explicitly-set
//!      invalid value is a bad invocation, not something to paper over.
//!   2. Else `dev` iff the running executable sits under a cargo `target`
//!      directory (a `target`-named ancestor containing a `CACHEDIR.TAG`,
//!      which cargo always writes).
//!   3. Else `live`.

use std::sync::OnceLock;

use camino::{Utf8Path, Utf8PathBuf};

use crate::cache::error::CacheError;

/// Environment override for the resolved cache channel.
pub(crate) const CHANNEL_ENV: &str = "NORN_CACHE_CHANNEL";

/// The cache isolation channel a process resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Channel {
    /// The installed binary's namespace: `<cache_home>/norn/<hash>/cache.db`.
    Live,
    /// A cargo-build-tree binary's namespace, nested inside the same vault
    /// entry: `<cache_home>/norn/<hash>/dev/cache.db`.
    Dev,
}

impl Channel {
    /// Operator-facing label (`norn cache status`, JSON output).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Channel::Live => "live",
            Channel::Dev => "dev",
        }
    }

    /// Subdirectory (relative to the shared vault entry dir) that houses this
    /// channel's database, or `None` when the database sits directly in the
    /// entry dir. The write lock and vault-level state stay in the entry dir
    /// regardless of channel, so only the database moves.
    pub(crate) fn db_subdir(self) -> Option<&'static str> {
        match self {
            Channel::Live => None,
            Channel::Dev => Some("dev"),
        }
    }
}

/// The process cache channel, resolved once. Errors only when
/// `NORN_CACHE_CHANNEL` is set to an unrecognized value; the error carries the
/// offending value and is reproduced (not cached-as-error-object) on each call.
pub(crate) fn channel() -> Result<Channel, CacheError> {
    static RESOLVED: OnceLock<Result<Channel, String>> = OnceLock::new();
    RESOLVED
        .get_or_init(|| resolve_channel(std::env::var(CHANNEL_ENV).ok(), detect_dev_from_exe()))
        .clone()
        .map_err(|value| CacheError::InvalidCacheChannel { value })
}

/// Pure resolution given the (optional) env value and whether the executable
/// was detected under a cargo target dir. Factored out for unit tests so the
/// once-per-process [`channel`] cache never has to be defeated.
///
/// `Err(value)` carries the offending env value verbatim for the error message.
fn resolve_channel(env: Option<String>, exe_under_target: bool) -> Result<Channel, String> {
    if let Some(raw) = env {
        return match raw.as_str() {
            "live" => Ok(Channel::Live),
            "dev" => Ok(Channel::Dev),
            _ => Err(raw),
        };
    }
    if exe_under_target {
        Ok(Channel::Dev)
    } else {
        Ok(Channel::Live)
    }
}

/// Whether `std::env::current_exe()` sits under a cargo target directory.
/// Best-effort: an unresolvable / non-UTF-8 exe path reports `false` (live).
fn detect_dev_from_exe() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| Utf8PathBuf::from_path_buf(p).ok())
        .map(|exe| {
            exe_under_cargo_target(&exe, |dir| dir.join("CACHEDIR.TAG").as_std_path().exists())
        })
        .unwrap_or(false)
}

/// Walk `exe`'s ancestors for a directory named `target` that `probe` confirms
/// contains a `CACHEDIR.TAG`. Cargo writes that tag into every target dir, so
/// the tag probe rejects false positives from unrelated path components merely
/// named `target`. `probe` is injected so tests exercise the walk without a
/// real filesystem.
fn exe_under_cargo_target(exe: &Utf8Path, probe: impl Fn(&Utf8Path) -> bool) -> bool {
    exe.ancestors()
        .any(|ancestor| ancestor.file_name() == Some("target") && probe(ancestor))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_live_wins() {
        assert_eq!(
            resolve_channel(Some("live".to_string()), true),
            Ok(Channel::Live)
        );
    }

    #[test]
    fn env_dev_wins() {
        assert_eq!(
            resolve_channel(Some("dev".to_string()), false),
            Ok(Channel::Dev)
        );
    }

    #[test]
    fn env_invalid_value_errors() {
        assert_eq!(
            resolve_channel(Some("prod".to_string()), false),
            Err("prod".to_string())
        );
    }

    #[test]
    fn env_empty_value_errors() {
        // An explicitly-set empty value is a bad invocation, not "unset".
        assert_eq!(
            resolve_channel(Some(String::new()), true),
            Err(String::new())
        );
    }

    #[test]
    fn default_dev_when_under_target() {
        assert_eq!(resolve_channel(None, true), Ok(Channel::Dev));
    }

    #[test]
    fn default_live_when_not_under_target() {
        assert_eq!(resolve_channel(None, false), Ok(Channel::Live));
    }

    #[test]
    fn detection_requires_cachedir_tag() {
        let exe = Utf8Path::new("/home/u/proj/target/debug/norn");
        // A plain `target` ancestor without the tag is not a cargo target dir.
        assert!(!exe_under_cargo_target(exe, |_| false));
        // With the tag present cargo target dir is confirmed.
        assert!(exe_under_cargo_target(exe, |dir| dir == "/home/u/proj/target"));
    }

    #[test]
    fn detection_ignores_unrelated_target_component() {
        // A directory literally named `target` deep in an installed path must
        // not trip detection unless it carries a CACHEDIR.TAG.
        let exe = Utf8Path::new("/opt/target/bin/norn");
        assert!(!exe_under_cargo_target(exe, |_| false));
    }

    #[test]
    fn detection_matches_only_target_named_dir() {
        // The tag probe is only consulted for `target`-named ancestors, so a
        // tag elsewhere never triggers a match.
        let exe = Utf8Path::new("/home/u/proj/build/debug/norn");
        assert!(!exe_under_cargo_target(exe, |_| true));
    }

    #[test]
    fn db_subdir_layout() {
        assert_eq!(Channel::Live.db_subdir(), None);
        assert_eq!(Channel::Dev.db_subdir(), Some("dev"));
    }
}
