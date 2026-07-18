//! Brand-token-aligned color palette for norn output.
//!
//! [`Palette`] maps the norn-brand token set to `anstyle::Style` values.
//! Use [`resolve`] to get a palette calibrated to the current environment
//! (TTY detection, `NO_COLOR`, `CLICOLOR_FORCE`). Ported from the donor
//! `src/output/palette.rs` (retired tree) for the custom help renderer.

use std::env;
use std::io::IsTerminal;

use anstyle::{Ansi256Color, Color, Style};

use crate::cli::ColorWhen;

/// Brand-token color palette.
///
/// Every field is an `anstyle::Style`. When color is disabled (`enabled ==
/// false`) every style is `Style::new()` (a no-op). When color is enabled
/// the styles carry ANSI-256 color codes drawn from the norn-brand token set.
/// Trimmed to the tokens the custom help renderer consumes (`bone`, `thread`,
/// `moss`, `dim`, `section`); the donor's full brand set (`amber`, `rune`,
/// `label`, `header`) ports with the record-block primitives, not the help
/// surface.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// Default foreground — terminal default, no override.
    pub bone: Style,
    /// Accent (ANSI 256 color 67).
    pub thread: Style,
    /// Success (ANSI 256 color 108).
    pub moss: Style,
    /// Muted — ANSI 256 #244 (#808080, medium gray) per brand §2.
    pub dim: Style,
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
            dim: Style::new(),
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
            dim,
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
}
