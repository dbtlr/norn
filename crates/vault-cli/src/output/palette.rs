//! Brand-token-aligned color palette for vault-cli output.
//!
//! [`Palette`] maps the norn-brand token set to `anstyle::Style` values.
//! Use [`resolve`] to get a palette calibrated to the current environment
//! (TTY detection, `NO_COLOR`, `CLICOLOR_FORCE`).

use std::env;
use std::io::IsTerminal;

use anstyle::{Ansi256Color, Color, Style};

use crate::cli::ColorWhen;

/// Brand-token color palette.
///
/// Every field is an `anstyle::Style`. When color is disabled (`enabled ==
/// false`) every style is `Style::new()` (a no-op). When color is enabled
/// the styles carry ANSI-256 color codes drawn from the norn-brand token set.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// Default foreground — terminal default, no override.
    pub bone: Style,
    /// Accent (ANSI 256 color 67).
    pub thread: Style,
    /// Success (ANSI 256 color 108).
    pub moss: Style,
    /// Warning (ANSI 256 color 178).
    pub amber: Style,
    /// Error (ANSI 256 color 167).
    pub rune: Style,
    /// Muted — `Style::new().dimmed()`.
    pub dim: Style,
    /// Field labels (= `dim`).
    pub label: Style,
    /// Section headers (= `dim().bold()`). Used by future grouped-tally commands.
    #[allow(dead_code)]
    pub section: Style,
    /// Whether color output is enabled. Read by future commands that need to
    /// branch on color state (e.g. suppress ANSI control chars in paths output).
    #[allow(dead_code)]
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
            section: Style::new(),
            enabled: false,
        }
    }

    /// Returns the full brand-token palette with ANSI 256 colors applied.
    pub const fn on() -> Self {
        let dim = Style::new().dimmed();
        Self {
            bone: Style::new(),
            thread: ansi256(67),
            moss: ansi256(108),
            amber: ansi256(178),
            rune: ansi256(167),
            dim,
            label: dim,
            section: dim.bold(),
            enabled: true,
        }
    }
}

/// Resolve a [`Palette`] for the given `ColorWhen` setting.
///
/// Reads `NO_COLOR` and `CLICOLOR_FORCE` from the environment and detects
/// whether stdout is a TTY, then delegates to [`resolve_inner`].
pub fn resolve(when: ColorWhen) -> Palette {
    let no_color = env::var_os("NO_COLOR").is_some();
    let force = env::var_os("CLICOLOR_FORCE").is_some();
    let is_tty = std::io::stdout().is_terminal();
    resolve_inner(when, no_color, force, is_tty)
}

/// Inner resolution logic — separated for testability.
///
/// `no_color`: `NO_COLOR` env var is set.
/// `force`: `CLICOLOR_FORCE` env var is set.
/// `is_tty`: stdout is a terminal.
pub(crate) fn resolve_inner(when: ColorWhen, no_color: bool, force: bool, is_tty: bool) -> Palette {
    match when {
        ColorWhen::Always => Palette::on(),
        ColorWhen::Never => Palette::off(),
        ColorWhen::Auto => {
            if no_color {
                Palette::off()
            } else if force || is_tty {
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
        assert_eq!(format!("{}", p.rune.render()), "");
    }

    #[test]
    fn on_severity_styles_render_ansi() {
        let p = Palette::on();
        assert!(p.enabled);
        assert_ne!(format!("{}", p.moss.render()), "");
        assert_ne!(format!("{}", p.amber.render()), "");
        assert_ne!(format!("{}", p.rune.render()), "");
        assert_ne!(format!("{}", p.thread.render()), "");
    }

    #[test]
    fn resolve_always_returns_on() {
        assert!(resolve(ColorWhen::Always).enabled);
    }

    #[test]
    fn resolve_never_returns_off() {
        assert!(!resolve(ColorWhen::Never).enabled);
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
}
