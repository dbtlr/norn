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
    !effective_locale().to_lowercase().contains("utf")
}

/// The effective POSIX locale string for glyph selection (NRN-336).
///
/// POSIX precedence: `LC_ALL` overrides `LC_CTYPE`, which overrides `LANG`. A
/// variable set to the empty string is treated as UNSET (POSIX: an empty value
/// does not select a locale) — the previous port read `LC_ALL` unconditionally,
/// so an empty `LC_ALL` masked a real `LANG=…UTF-8` and forced the ASCII
/// fallback. Each level is consulted in turn; the first nonempty value wins,
/// and an all-unset environment yields `""` (→ ASCII fallback).
///
/// TTY-only in effect: glyph rendering only differs on an interactive terminal,
/// and the parity harness runs piped, so this precedence is not pinnable by a
/// parity case — hence no ledger entry, only the precedence unit tests below.
fn effective_locale() -> String {
    for key in ["LC_ALL", "LC_CTYPE", "LANG"] {
        match std::env::var(key) {
            Ok(val) if !val.is_empty() => return val,
            _ => continue,
        }
    }
    String::new()
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

    /// Env vars are process-global; serialize the locale-precedence assertions
    /// so parallel test threads cannot observe each other's mutations.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Set (or, with `None`, clear) the four glyph-relevant env vars, run `f`,
    /// then restore the prior values — so the matrix cannot leak across tests.
    fn with_env(vars: &[(&str, Option<&str>)], f: impl FnOnce()) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let keys = ["NORN_ASCII", "LC_ALL", "LC_CTYPE", "LANG"];
        let saved: Vec<(&str, Option<String>)> =
            keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
        for key in keys {
            std::env::remove_var(key);
        }
        for (key, val) in vars {
            match val {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        f();
        for (key, val) in saved {
            match val {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    fn locale_precedence_matrix() {
        // LC_ALL (nonempty) wins over LC_CTYPE and LANG.
        with_env(
            &[
                ("LC_ALL", Some("en_US.UTF-8")),
                ("LC_CTYPE", Some("C")),
                ("LANG", Some("C")),
            ],
            || assert!(!use_ascii(), "nonempty UTF LC_ALL must select UTF glyphs"),
        );

        // Empty LC_ALL is treated as unset — LC_CTYPE is consulted next.
        with_env(
            &[
                ("LC_ALL", Some("")),
                ("LC_CTYPE", Some("en_US.UTF-8")),
                ("LANG", Some("C")),
            ],
            || {
                assert!(
                    !use_ascii(),
                    "empty LC_ALL must fall through to a UTF LC_CTYPE"
                )
            },
        );

        // Empty LC_ALL and LC_CTYPE both fall through to LANG.
        with_env(
            &[
                ("LC_ALL", Some("")),
                ("LC_CTYPE", Some("")),
                ("LANG", Some("en_US.UTF-8")),
            ],
            || assert!(!use_ascii(), "empty LC_ALL/LC_CTYPE fall through to LANG"),
        );

        // A non-UTF LC_ALL wins even when LANG is UTF.
        with_env(
            &[("LC_ALL", Some("C")), ("LANG", Some("en_US.UTF-8"))],
            || assert!(use_ascii(), "non-UTF LC_ALL must force the ASCII fallback"),
        );

        // All unset → ASCII fallback.
        with_env(&[], || {
            assert!(
                use_ascii(),
                "an all-unset locale must use the ASCII fallback"
            )
        });

        // NORN_ASCII forces ASCII regardless of a UTF locale.
        with_env(
            &[("NORN_ASCII", Some("1")), ("LC_ALL", Some("en_US.UTF-8"))],
            || assert!(use_ascii(), "NORN_ASCII overrides the locale"),
        );
    }
}
