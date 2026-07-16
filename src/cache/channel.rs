//! Cache channel: `live` vs `dev` isolation.
//!
//! A binary running from a cargo build tree (`cargo run`, `cargo test`, a
//! freshly-built `target/…/norn`) must never read, migrate, invalidate, or
//! write the cache namespace the installed (live) binary uses. `Cache::open`'s
//! older-schema silent rebuild otherwise lets a dev binary migrate the live
//! per-vault cache out from under an installed client, locking it out with the
//! upgrade-required error.
//!
//! Path-based runtime detection alone cannot survive the binary being copied
//! elsewhere (NRN-285): a `target/release/norn` copied to, say, `/tmp` carries
//! none of its build tree's ancestry, so the walk below finds no
//! `CACHEDIR.TAG` and misresolves a dev build to `live` — silently migrating
//! the installed binary's cache. Two more layers close that gap: a channel
//! baked into the binary at compile time (`build.rs`, survives copying
//! because it travels with the binary rather than living in its path) and a
//! runtime check for temp-dir residency (catches a copy that predates the
//! baked-channel layer, or a build where nothing was baked).
//!
//! Resolution order (resolved once per process; see [`channel`]):
//!   1. `NORN_CACHE_CHANNEL` env var — exactly `live` or `dev`; any other
//!      non-empty value is a hard error (an explicitly-set invalid value is a
//!      bad invocation, not something to paper over); empty counts as unset.
//!   2. Else the channel baked at compile time via `option_env!` (see
//!      `build.rs`'s module doc for the bake rule) — `live` or `dev` as
//!      baked. An unrecognized baked value (defensive only; `build.rs`
//!      validates before baking) is treated as absent rather than a hard
//!      error, since this layer only ever narrows the resolution.
//!   3. Else `dev` iff the running executable sits under a cargo build tree
//!      (any ancestor containing a `CACHEDIR.TAG` file, which cargo writes
//!      into every target directory root regardless of its name) OR under a
//!      system temp location (the binary-copy case above), failing toward
//!      `dev` when the exe path itself can't be resolved.
//!   4. Else `live`.

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
        .get_or_init(|| {
            resolve_channel(
                std::env::var(CHANNEL_ENV).ok(),
                option_env!("NORN_BAKED_CHANNEL"),
                detect_dev_from_exe(),
            )
        })
        .clone()
        .map_err(|value| CacheError::InvalidCacheChannel { value })
}

/// Pure resolution given the (optional) env value, the compile-time-baked
/// channel (if any), and whether the executable was detected as a dev build
/// at runtime. Factored out for unit tests so the once-per-process
/// [`channel`] cache never has to be defeated.
///
/// A set-but-empty env value counts as unset (standard env-var convention) and
/// falls through to the baked value / detection. `Err(value)` carries the
/// offending non-empty env value verbatim for the error message. An
/// unrecognized `baked` value (see the module doc's layer 2) is treated the
/// same as absent — it falls through to `detected_dev` rather than erroring.
fn resolve_channel(
    env: Option<String>,
    baked: Option<&str>,
    detected_dev: bool,
) -> Result<Channel, String> {
    match env.as_deref() {
        Some("live") => return Ok(Channel::Live),
        Some("dev") => return Ok(Channel::Dev),
        Some("") | None => {}
        Some(_) => return Err(env.unwrap()),
    }
    match baked {
        Some("live") => return Ok(Channel::Live),
        Some("dev") => return Ok(Channel::Dev),
        Some(_) | None => {}
    }
    if detected_dev {
        Ok(Channel::Dev)
    } else {
        Ok(Channel::Live)
    }
}

/// Whether `std::env::current_exe()` sits under a cargo build tree or a
/// system temp location.
///
/// Fails toward `dev`: an unresolvable or non-UTF-8 exe path yields `true`,
/// because the dangerous misclassification direction is a dev binary landing
/// on the live channel (it can silently migrate the installed client's cache);
/// a live binary landing on dev merely gets a harmlessly isolated cache.
///
/// The exe path is canonicalized before the temp-prefix comparison (falling
/// back to the raw path if that fails, e.g. because the binary was since
/// deleted) — `/tmp` is a symlink to `/private/tmp` on macOS, so an
/// uncanonicalized path would miss the literal-prefix match below.
fn detect_dev_from_exe() -> bool {
    let exe = std::env::current_exe()
        .ok()
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p));
    detect_dev(
        exe.and_then(|p| Utf8PathBuf::from_path_buf(p).ok())
            .as_deref(),
        |dir| dir.join("CACHEDIR.TAG").as_std_path().exists(),
        &temp_dir_prefixes(),
    )
}

/// System temp-dir prefixes an exe path is checked against (NRN-285): a
/// binary copied out of its build tree (e.g. into `/tmp` for an ad hoc
/// benchmark) carries no `CACHEDIR.TAG` ancestor, so this is the fallback
/// that still catches it. `std::env::temp_dir()` is canonicalized because it
/// varies by platform and can itself be a symlink (macOS: a per-user
/// `/var/folders/.../T` staging dir reached through `/tmp` ->
/// `/private/tmp`); the literal well-known roots are checked alongside it
/// since the canonicalized value alone won't cover every temp location a
/// caller might use directly (e.g. `/var/tmp`).
fn temp_dir_prefixes() -> Vec<Utf8PathBuf> {
    let mut prefixes = Vec::new();
    let temp = std::env::temp_dir();
    let temp = std::fs::canonicalize(&temp).unwrap_or(temp);
    if let Ok(temp) = Utf8PathBuf::from_path_buf(temp) {
        prefixes.push(temp);
    }
    for literal in ["/tmp", "/private/tmp", "/var/tmp", "/var/folders"] {
        prefixes.push(Utf8PathBuf::from(literal));
    }
    prefixes
}

/// Detection core behind [`detect_dev_from_exe`], with the (possibly
/// unresolvable) exe path, filesystem probe, and temp-dir prefixes injected
/// for unit tests. `None` — the exe path couldn't be resolved — reports dev
/// (fail-safe; see [`detect_dev_from_exe`]).
fn detect_dev(
    exe: Option<&Utf8Path>,
    probe: impl Fn(&Utf8Path) -> bool,
    temp_prefixes: &[Utf8PathBuf],
) -> bool {
    match exe {
        Some(exe) => {
            exe_under_cargo_build_tree(exe, probe) || exe_under_temp_prefix(exe, temp_prefixes)
        }
        None => true,
    }
}

/// Whether `exe` sits under any of `temp_prefixes`.
fn exe_under_temp_prefix(exe: &Utf8Path, temp_prefixes: &[Utf8PathBuf]) -> bool {
    temp_prefixes.iter().any(|prefix| exe.starts_with(prefix))
}

/// Walk `exe`'s ancestors for one that `probe` confirms contains a
/// `CACHEDIR.TAG` file. Cargo writes the tag into every target directory root
/// regardless of its name, so this catches custom `CARGO_TARGET_DIR` /
/// `build.target-dir` locations whose leaf is not named `target`. Standard
/// installs stay live: `~/.cargo` itself carries no tag (the tagged
/// `registry/` and `git/` dirs are siblings of `bin/`, not ancestors), and
/// system prefixes like `/usr/local/bin` have no tagged ancestor. `probe` is
/// injected so tests exercise the walk without a real filesystem.
fn exe_under_cargo_build_tree(exe: &Utf8Path, probe: impl Fn(&Utf8Path) -> bool) -> bool {
    exe.ancestors().any(probe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_live_wins_over_detection() {
        assert_eq!(
            resolve_channel(Some("live".to_string()), None, true),
            Ok(Channel::Live)
        );
    }

    #[test]
    fn env_dev_wins_over_detection() {
        assert_eq!(
            resolve_channel(Some("dev".to_string()), None, false),
            Ok(Channel::Dev)
        );
    }

    #[test]
    fn env_invalid_value_errors() {
        assert_eq!(
            resolve_channel(Some("prod".to_string()), None, false),
            Err("prod".to_string())
        );
    }

    #[test]
    fn env_empty_value_falls_through_to_detection() {
        // A set-but-empty value counts as unset (standard env-var convention):
        // detection decides, in both directions.
        assert_eq!(
            resolve_channel(Some(String::new()), None, true),
            Ok(Channel::Dev)
        );
        assert_eq!(
            resolve_channel(Some(String::new()), None, false),
            Ok(Channel::Live)
        );
    }

    #[test]
    fn env_beats_baked_live_over_baked_dev() {
        assert_eq!(
            resolve_channel(Some("live".to_string()), Some("dev"), false),
            Ok(Channel::Live)
        );
    }

    #[test]
    fn env_beats_baked_dev_over_baked_live() {
        assert_eq!(
            resolve_channel(Some("dev".to_string()), Some("live"), true),
            Ok(Channel::Dev)
        );
    }

    #[test]
    fn baked_beats_detection_dev_over_detected_live() {
        assert_eq!(resolve_channel(None, Some("dev"), false), Ok(Channel::Dev));
    }

    #[test]
    fn baked_beats_detection_live_over_detected_dev() {
        assert_eq!(resolve_channel(None, Some("live"), true), Ok(Channel::Live));
    }

    #[test]
    fn unrecognized_baked_value_falls_through_to_detection() {
        // Defensive only: build.rs validates before baking, but a malformed
        // baked value must not panic — it just loses its say, in both
        // directions.
        assert_eq!(
            resolve_channel(None, Some("staging"), true),
            Ok(Channel::Dev)
        );
        assert_eq!(
            resolve_channel(None, Some("staging"), false),
            Ok(Channel::Live)
        );
    }

    #[test]
    fn default_dev_when_under_target() {
        assert_eq!(resolve_channel(None, None, true), Ok(Channel::Dev));
    }

    #[test]
    fn default_live_when_not_under_target() {
        assert_eq!(resolve_channel(None, None, false), Ok(Channel::Live));
    }

    #[test]
    fn detection_requires_cachedir_tag() {
        let exe = Utf8Path::new("/home/u/proj/target/debug/norn");
        // No tagged ancestor anywhere: not a cargo build tree, even with a
        // `target`-named path component.
        assert!(!exe_under_cargo_build_tree(exe, |_| false));
        // With the tag present on the target root, the build tree is confirmed.
        assert!(exe_under_cargo_build_tree(exe, |dir| dir == "/home/u/proj/target"));
    }

    #[test]
    fn detection_covers_custom_named_target_dir() {
        // CARGO_TARGET_DIR / build.target-dir may point anywhere; cargo still
        // writes CACHEDIR.TAG into the root, so the leaf name is irrelevant.
        let exe = Utf8Path::new("/home/u/builds/norn-out/debug/norn");
        assert!(exe_under_cargo_build_tree(exe, |dir| dir == "/home/u/builds/norn-out"));
        // And absent the tag, the same path is not a build tree.
        assert!(!exe_under_cargo_build_tree(exe, |_| false));
    }

    #[test]
    fn detection_untagged_install_path_is_live() {
        // A standard install location has no tagged ancestor (~/.cargo carries
        // no CACHEDIR.TAG; registry/ and git/ are siblings of bin/, not
        // ancestors), so the default stays live.
        let exe = Utf8Path::new("/home/u/.cargo/bin/norn");
        assert!(!exe_under_cargo_build_tree(exe, |_| false));
    }

    #[test]
    fn detection_unresolvable_exe_fails_toward_dev() {
        // current_exe() failure / non-UTF-8 path: the fail-safe direction is
        // dev (an isolated cache), never live (cross-channel migration risk).
        assert!(detect_dev(None, |_| false, &[]));
    }

    #[test]
    fn detection_temp_dir_residency_counts_as_dev() {
        // A binary copied out of its build tree (NRN-285's incident shape)
        // carries no CACHEDIR.TAG ancestor, but landing under an injected
        // temp prefix still resolves dev.
        let exe = Utf8Path::new("/tmp/norn-after/norn");
        let prefixes = vec![Utf8PathBuf::from("/tmp")];
        assert!(detect_dev(Some(exe), |_| false, &prefixes));
    }

    #[test]
    fn detection_no_temp_prefix_and_no_tag_is_live() {
        let exe = Utf8Path::new("/usr/local/bin/norn");
        let prefixes = vec![Utf8PathBuf::from("/tmp")];
        assert!(!detect_dev(Some(exe), |_| false, &prefixes));
    }

    #[test]
    fn detection_cachedir_tag_walk_still_works_alongside_temp_check() {
        // The temp-prefix addition must not shadow the existing build-tree walk.
        let exe = Utf8Path::new("/home/u/proj/target/debug/norn");
        let prefixes = vec![Utf8PathBuf::from("/tmp")];
        assert!(detect_dev(
            Some(exe),
            |dir| dir == "/home/u/proj/target",
            &prefixes
        ));
    }

    #[test]
    fn db_subdir_layout() {
        assert_eq!(Channel::Live.db_subdir(), None);
        assert_eq!(Channel::Dev.db_subdir(), Some("dev"));
    }
}
