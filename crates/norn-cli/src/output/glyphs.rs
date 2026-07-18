//! Glyph rendering — UTF-8 symbols with ASCII fallbacks.
//!
//! Trimmed port of the donor `src/output/glyphs.rs` (retired tree): only the
//! glyphs the custom help renderer references are carried over — the live-example
//! marker and its separator dot. `use_ascii()` probes the environment for the
//! caller's preferred mode.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    /// Separator dot. UTF: `·` (MIDDLE DOT). ASCII fallback: `.`.
    Sep,
    /// Live-example marker. UTF: `▸` (BLACK RIGHT-POINTING SMALL TRIANGLE).
    /// ASCII fallback: `>`.
    Marker,
}

pub fn render(g: Glyph, ascii: bool) -> &'static str {
    match (g, ascii) {
        (Glyph::Sep, false) => "·",
        (Glyph::Sep, true) => ".",
        (Glyph::Marker, false) => "▸",
        (Glyph::Marker, true) => ">",
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
    fn sep_utf_and_ascii() {
        assert_eq!(render(Glyph::Sep, false), "·");
        assert_eq!(render(Glyph::Sep, true), ".");
    }

    #[test]
    fn marker_utf_and_ascii() {
        assert_eq!(render(Glyph::Marker, false), "▸");
        assert_eq!(render(Glyph::Marker, true), ">");
    }
}
