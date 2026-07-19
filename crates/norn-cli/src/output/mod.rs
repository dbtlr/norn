//! Output vocabulary for the custom help renderer: the brand color palette
//! and the glyph set. Ported from the donor `src/output/` (retired tree) —
//! only the pieces the help renderer depends on (`palette`, `glyphs`). The
//! record-block primitives, projection, and pager port with the read verbs
//! (separate burn-down rows), not with the help surface.

pub mod glyphs;
pub mod palette;
pub mod primitives;
pub mod projection;

/// Whether a boolean toggle environment variable is set to an *effective*
/// value — present **and** non-empty.
///
/// This is the single enforcement point for the POSIX-by-default env-var rule
/// (ADR 0020) applied to the CLI's boolean toggles (`NO_COLOR`, `NORN_ASCII`,
/// and the base predicate under `CLICOLOR_FORCE`). POSIX treats a variable set
/// to the empty string as *unset* for the purpose of selecting behavior; the
/// no-color.org convention says the same explicitly ("present, and not an empty
/// string"). A bare `NO_COLOR=` on the command line is therefore a no-op, not a
/// force-off — matching every other conformant tool.
///
/// Value-carrying knobs (a TTL, a path) are read directly at their sites with
/// their own empty/invalid adjudication; this helper is only for the presence
/// toggles, where any non-empty value means "on".
pub(crate) fn env_flag(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

#[cfg(test)]
mod env_flag_tests {
    use super::env_flag;
    use crate::test_support::EnvGuard;

    #[test]
    fn present_nonempty_is_true() {
        let _env = EnvGuard::new(&[("NORN_ENV_FLAG_PROBE", Some("1"))]);
        assert!(env_flag("NORN_ENV_FLAG_PROBE"));
    }

    #[test]
    fn present_empty_is_false() {
        // POSIX / no-color.org: an empty value is treated as unset.
        let _env = EnvGuard::new(&[("NORN_ENV_FLAG_PROBE", Some(""))]);
        assert!(!env_flag("NORN_ENV_FLAG_PROBE"));
    }

    #[test]
    fn absent_is_false() {
        let _env = EnvGuard::new(&[("NORN_ENV_FLAG_PROBE", None)]);
        assert!(!env_flag("NORN_ENV_FLAG_PROBE"));
    }

    #[test]
    fn any_nonempty_value_is_true() {
        // Value content is irrelevant for a presence toggle — only emptiness.
        let _env = EnvGuard::new(&[("NORN_ENV_FLAG_PROBE", Some("anything"))]);
        assert!(env_flag("NORN_ENV_FLAG_PROBE"));
    }
}
