//! Brand-token-aligned color palette for norn output.
//!
//! [`Palette`] maps the norn-brand token set to `anstyle::Style` values.
//! Use [`resolve`] to get a palette calibrated to the current environment
//! (TTY detection, `NO_COLOR`, `CLICOLOR_FORCE`).

use std::io::IsTerminal;

use anstyle::{Ansi256Color, Color, Style};

use crate::cli::ColorWhen;
use crate::output::env_flag;

/// Brand-token color palette.
///
/// Every field is an `anstyle::Style`. When color is disabled (`enabled ==
/// false`) every style is `Style::new()` (a no-op). When color is enabled
/// the styles carry ANSI-256 color codes drawn from the norn-brand token set.
/// Trimmed to the tokens the custom help renderer consumes (`bone`, `thread`,
/// `moss`, `dim`, `section`); the fuller brand set (`amber`, `rune`,
/// `label`, `header`) lives with the record-block primitives, not the help
/// surface.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// Default foreground — terminal default, no override.
    pub bone: Style,
    /// Accent (ANSI 256 color 67).
    pub thread: Style,
    /// Success (ANSI 256 color 108).
    pub moss: Style,
    /// Warning (ANSI 256 color 178) — the validate severity tally + finding glyph.
    pub amber: Style,
    /// Error (ANSI 256 color 167) — the validate severity tally + finding glyph.
    pub rune: Style,
    /// Muted — ANSI 256 #244 (#808080, medium gray) per brand §2.
    pub dim: Style,
    /// Field labels in a record block (= `dim`).
    pub label: Style,
    /// Record-block headers (= `bone.bold()`).
    pub header: Style,
    /// Section headers (= `dim().bold()`).
    pub section: Style,
    /// Whether color output is enabled.
    pub enabled: bool,
}

const fn ansi256(n: u8) -> Style {
    Style::new().fg_color(Some(Color::Ansi256(Ansi256Color(n))))
}

impl Palette {
    /// Returns a palette with no styling — every field is `Style::new()`.
    pub const fn off() -> Self {
        Self {
            bone: Style::new(),
            thread: Style::new(),
            moss: Style::new(),
            amber: Style::new(),
            rune: Style::new(),
            dim: Style::new(),
            label: Style::new(),
            header: Style::new(),
            section: Style::new(),
            enabled: false,
        }
    }

    /// Returns `true` when this palette has all styles disabled (no-color path).
    pub const fn is_off(&self) -> bool {
        !self.enabled
    }

    /// Returns the full brand-token palette with ANSI 256 colors applied.
    pub const fn on() -> Self {
        // `dim` and `bone` ship as explicit ANSI 256 colors per norn-brand
        // §2 — NOT as SGR effects, because many terminals silently ignore
        // SGR 2 ("faint") and render text as the terminal default, defeating
        // the visual distinction between bone (foreground) and dim (muted).
        let dim = ansi256(244);
        let bone = ansi256(253);
        Self {
            bone,
            thread: ansi256(67),
            moss: ansi256(108),
            amber: ansi256(178),
            rune: ansi256(167),
            dim,
            label: dim,
            header: bone.bold(),
            section: dim.bold(),
            enabled: true,
        }
    }
}

/// Resolve a [`Palette`] for the given `ColorWhen` setting.
///
/// Reads `NO_COLOR` and `CLICOLOR_FORCE` from the environment and detects
/// whether stdout is a TTY, then delegates to [`resolve_inner`].
///
/// Env-var semantics follow the POSIX-by-default rule (ADR 0020): an *empty*
/// value is treated as unset for both toggles (a bare `NO_COLOR=` does not
/// disable color), per the no-color.org convention. `CLICOLOR_FORCE` follows
/// the CLICOLOR spec's `!= "0"` rule on top of that — `CLICOLOR_FORCE=0` does
/// not force color.
pub fn resolve(when: ColorWhen) -> Palette {
    let no_color = env_flag("NO_COLOR");
    let force = clicolor_force();
    let is_tty = std::io::stdout().is_terminal();
    resolve_inner(when, no_color, force, is_tty)
}

/// Whether `CLICOLOR_FORCE` requests forced color: present, non-empty, and not
/// literally `0` (the CLICOLOR convention, https://bixense.com/clicolors/).
fn clicolor_force() -> bool {
    match std::env::var_os("CLICOLOR_FORCE") {
        Some(value) => !value.is_empty() && value != "0",
        None => false,
    }
}

/// Inner resolution logic — separated for testability.
pub(crate) fn resolve_inner(when: ColorWhen, no_color: bool, force: bool, is_tty: bool) -> Palette {
    // NO_COLOR takes precedence over everything, including --color always.
    // See https://no-color.org/
    if no_color {
        return Palette::off();
    }
    match when {
        ColorWhen::Always => Palette::on(),
        ColorWhen::Never => Palette::off(),
        ColorWhen::Auto => {
            if force || is_tty {
                Palette::on()
            } else {
                Palette::off()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_has_zero_styles_and_disabled_flag() {
        let p = Palette::off();
        assert!(!p.enabled);
        assert_eq!(format!("{}", p.thread.render()), "");
        assert_eq!(format!("{}", p.section.render()), "");
    }

    #[test]
    fn on_styles_render_ansi() {
        let p = Palette::on();
        assert!(p.enabled);
        assert_ne!(format!("{}", p.moss.render()), "");
        assert_ne!(format!("{}", p.thread.render()), "");
        assert_ne!(format!("{}", p.section.render()), "");
    }

    #[test]
    fn resolve_always_without_no_color_returns_on() {
        assert!(resolve_inner(ColorWhen::Always, false, false, false).enabled);
    }

    #[test]
    fn resolve_always_with_no_color_returns_off() {
        assert!(!resolve_inner(ColorWhen::Always, true, false, false).enabled);
    }

    #[test]
    fn resolve_never_returns_off() {
        assert!(!resolve_inner(ColorWhen::Never, false, false, true).enabled);
    }

    #[test]
    fn resolve_inner_no_color_env_forces_off() {
        assert!(!resolve_inner(ColorWhen::Auto, true, false, true).enabled);
    }

    #[test]
    fn resolve_inner_clicolor_force_overrides_no_tty() {
        assert!(resolve_inner(ColorWhen::Auto, false, true, false).enabled);
    }

    #[test]
    fn resolve_inner_auto_with_tty_returns_on() {
        assert!(resolve_inner(ColorWhen::Auto, false, false, true).enabled);
    }

    #[test]
    fn resolve_inner_auto_without_tty_returns_off() {
        assert!(!resolve_inner(ColorWhen::Auto, false, false, false).enabled);
    }

    use crate::test_support::EnvGuard;

    #[test]
    fn clicolor_force_present_nonempty_nonzero_forces() {
        let _env = EnvGuard::new(&[("CLICOLOR_FORCE", Some("1"))]);
        assert!(clicolor_force());
    }

    #[test]
    fn clicolor_force_zero_does_not_force() {
        // CLICOLOR spec: `CLICOLOR_FORCE=0` is explicitly "do not force".
        let _env = EnvGuard::new(&[("CLICOLOR_FORCE", Some("0"))]);
        assert!(!clicolor_force());
    }

    #[test]
    fn clicolor_force_empty_does_not_force() {
        // POSIX-by-default (ADR 0020): an empty value is unset.
        let _env = EnvGuard::new(&[("CLICOLOR_FORCE", Some(""))]);
        assert!(!clicolor_force());
    }

    #[test]
    fn clicolor_force_absent_does_not_force() {
        let _env = EnvGuard::new(&[("CLICOLOR_FORCE", None)]);
        assert!(!clicolor_force());
    }
}
