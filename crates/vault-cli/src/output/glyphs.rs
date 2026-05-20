//! Glyph rendering — UTF-8 symbols with ASCII fallbacks.
//!
//! Call `render(glyph, ascii)` to get the appropriate string for a glyph.
//! Use `use_ascii()` to probe the environment for the caller's preferred mode.

// Arrow/Add/Mod/Del are reserved for the repair-port `change_line` primitive
// (next port pass). Leader is reserved for future grouped-tally rows.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    Pass,
    Warn,
    Err,
    Sep,
    Arrow,
    Add,
    Mod,
    Del,
    Leader,
}

pub fn render(g: Glyph, ascii: bool) -> &'static str {
    match (g, ascii) {
        (Glyph::Pass, false) => "✓",
        (Glyph::Pass, true) => "[ok]",
        (Glyph::Warn, false) => "⚠",
        (Glyph::Warn, true) => "[warn]",
        (Glyph::Err, false) => "✗",
        (Glyph::Err, true) => "[err]",
        (Glyph::Sep, false) => "·",
        (Glyph::Sep, true) => ".",
        (Glyph::Arrow, false) => "→",
        (Glyph::Arrow, true) => "->",
        (Glyph::Add, _) => "+",
        (Glyph::Mod, _) => "~",
        (Glyph::Del, _) => "-",
        (Glyph::Leader, false) => "·",
        (Glyph::Leader, true) => ".",
    }
}

pub fn use_ascii() -> bool {
    if std::env::var_os("NORN_ASCII").is_some() {
        return true;
    }
    let locale =
        std::env::var("LC_ALL").unwrap_or_else(|_| std::env::var("LANG").unwrap_or_default());
    !locale.to_lowercase().contains("utf")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pass_utf_and_ascii() {
        assert_eq!(render(Glyph::Pass, false), "✓");
        assert_eq!(render(Glyph::Pass, true), "[ok]");
    }

    #[test]
    fn warn_utf_and_ascii() {
        assert_eq!(render(Glyph::Warn, false), "⚠");
        assert_eq!(render(Glyph::Warn, true), "[warn]");
    }

    #[test]
    fn err_utf_and_ascii() {
        assert_eq!(render(Glyph::Err, false), "✗");
        assert_eq!(render(Glyph::Err, true), "[err]");
    }

    #[test]
    fn sep_utf_and_ascii() {
        assert_eq!(render(Glyph::Sep, false), "·");
        assert_eq!(render(Glyph::Sep, true), ".");
    }

    #[test]
    fn arrow_utf_and_ascii() {
        assert_eq!(render(Glyph::Arrow, false), "→");
        assert_eq!(render(Glyph::Arrow, true), "->");
    }

    #[test]
    fn diff_glyphs_are_ascii_safe_in_both_modes() {
        assert_eq!(render(Glyph::Add, false), "+");
        assert_eq!(render(Glyph::Add, true), "+");
        assert_eq!(render(Glyph::Mod, false), "~");
        assert_eq!(render(Glyph::Mod, true), "~");
        assert_eq!(render(Glyph::Del, false), "-");
        assert_eq!(render(Glyph::Del, true), "-");
    }

    #[test]
    fn leader_utf_and_ascii() {
        assert_eq!(render(Glyph::Leader, false), "·");
        assert_eq!(render(Glyph::Leader, true), ".");
    }
}
