//! The in-flight progress line (NRN-512): the CLI's rendering of the owner's
//! `Progress` frames.
//!
//! Rendering lives here and only here (invariant 4) — `norn-client` reports
//! typed observations and this layer decides what a user sees. What it decides:
//!
//! - **stderr, never stdout.** Progress is conversation, not payload; nothing
//!   here can reach the payload stream, so a verb's stdout is exactly what it
//!   would be with no progress at all.
//! - **TTY only, and only a terminal that can erase.** A piped stderr gets
//!   nothing at all, because a transient redrawn line is meaningless in a log or
//!   a `2>&1` capture; `TERM=dumb` gets nothing either, because a terminal with
//!   no cursor addressing would receive the erase sequence as literal bytes and
//!   the "transient" line would pile up instead of redrawing.
//! - **One line, redrawn in place**, erased when the request ends. A long
//!   `norn apply` reports that it is alive without scrolling the terminal.
//!
//! The line is the one deliberate exemption to the closed stderr prefix
//! vocabulary (`docs/architecture.md`): it carries no prefix because it is not a
//! diagnostic — it exists only while it is being overwritten, and it erases
//! itself before anything else is written.

use std::io::Write;

use norn_client::ProgressSink;
use norn_wire::{Progress, ProgressPhase};

use crate::output::glyphs::{self, Glyph};

/// The progress sink for this process, or `None` when this stderr cannot carry
/// a self-erasing line — the whole capability gate, read exactly once here. A
/// `None` sink means the session keeps its discarding default: frames are still
/// consumed (that is what resets the client's silence budget), nothing is drawn.
pub fn stderr_progress_sink() -> Option<Box<dyn ProgressSink>> {
    draws_progress(
        std::io::IsTerminal::is_terminal(&std::io::stderr()),
        term_is_dumb(),
    )
    .then(|| {
        Box::new(ProgressLine::new(std::io::stderr(), glyphs::use_ascii())) as Box<dyn ProgressSink>
    })
}

/// Whether this stderr can carry the transient line: a terminal, and one that
/// can move the cursor. Separated from the environment reads so both halves of
/// the gate are testable.
fn draws_progress(is_tty: bool, dumb: bool) -> bool {
    is_tty && !dumb
}

/// Whether `TERM` declares a terminal with no cursor addressing. Read as a
/// VALUE, like the other capability toggles (`palette.rs`'s `CLICOLOR_FORCE`),
/// not as mere presence — every terminal sets `TERM`.
fn term_is_dumb() -> bool {
    std::env::var_os("TERM").is_some_and(|term| term == "dumb")
}

/// A single terminal line redrawn in place.
struct ProgressLine<W> {
    out: W,
    /// The glyph mode this process renders in, resolved once at construction
    /// through the one `use_ascii()` switch rather than hardcoded here.
    ascii: bool,
}

impl<W: Write> ProgressLine<W> {
    fn new(out: W, ascii: bool) -> Self {
        Self { out, ascii }
    }
}

impl<W: Write + Send> ProgressSink for ProgressLine<W> {
    fn progress(&mut self, progress: &Progress) {
        // `\r` returns to column 0 and `\x1b[K` erases what the previous, longer
        // update left behind. Both are safe here: this sink is only ever
        // constructed for a terminal that can address its cursor.
        //
        // Write errors are DISCARDED, deliberately and uniquely here. Progress
        // is advisory: a command must not fail, or change its exit code, because
        // a decoration could not be drawn. Every payload write still goes
        // through `render_outcome`'s one IO-error policy.
        let _ = write!(self.out, "\r{}\x1b[K", progress_text(progress, self.ascii));
        let _ = self.out.flush();
    }

    fn finished(&mut self) {
        let _ = write!(self.out, "\r\x1b[K");
        let _ = self.out.flush();
    }
}

/// The line's text for one observation. A pure function of the typed frame — no
/// message from the owner is ever echoed (invariant 2), so the wording is this
/// layer's and the phase tag is the only thing crossing the wire. The ellipsis
/// comes from the glyph set, so a non-UTF-8 terminal reads `warming...` rather
/// than a replacement character.
fn progress_text(progress: &Progress, ascii: bool) -> String {
    let (verb, unit) = match progress.phase {
        ProgressPhase::Warming => ("warming", "documents"),
        ProgressPhase::Reading => ("reading", "documents"),
        ProgressPhase::Applying => ("applying", "operations"),
    };
    let cont = glyphs::render(Glyph::Ellipsis, ascii);
    match (progress.done, progress.total) {
        (Some(done), Some(total)) => format!("{verb}{cont} {done}/{total} {unit}"),
        (Some(done), None) => format!("{verb}{cont} {done} {unit}"),
        (None, Some(total)) => format!("{verb}{cont} {total} {unit} planned"),
        (None, None) => format!("{verb}{cont}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(phase: ProgressPhase, done: Option<u64>, total: Option<u64>) -> String {
        progress_text(&Progress { phase, done, total }, false)
    }

    #[test]
    fn every_milestone_combination_reads_as_a_sentence() {
        assert_eq!(text(ProgressPhase::Warming, None, None), "warming…");
        assert_eq!(
            text(ProgressPhase::Reading, Some(431), None),
            "reading… 431 documents"
        );
        assert_eq!(
            text(ProgressPhase::Applying, Some(3), Some(12)),
            "applying… 3/12 operations"
        );
        // Total without progress must not read as "12 done".
        assert_eq!(
            text(ProgressPhase::Applying, None, Some(12)),
            "applying… 12 operations planned"
        );
        assert_eq!(text(ProgressPhase::Reading, None, None), "reading…");
    }

    /// The ellipsis goes through the one glyph switch, so a terminal that did
    /// not declare a UTF-8 charset reads ASCII rather than a mangled byte.
    #[test]
    fn the_ellipsis_follows_the_ascii_switch() {
        let observation = Progress::new(ProgressPhase::Applying).with_total(Some(12));
        assert_eq!(
            progress_text(&observation, true),
            "applying... 12 operations planned"
        );
        assert_eq!(
            progress_text(&observation, false),
            "applying… 12 operations planned"
        );
    }

    /// The capability gate, both halves: a pipe draws nothing (a transient line
    /// is meaningless in a capture) and neither does a terminal that cannot move
    /// its cursor — writing the erase sequence there would leave literal bytes
    /// on screen, which is worse than drawing nothing at all.
    #[test]
    fn only_a_cursor_addressable_terminal_draws() {
        assert!(draws_progress(true, false));
        assert!(!draws_progress(false, false), "a pipe draws nothing");
        assert!(!draws_progress(true, true), "TERM=dumb draws nothing");
        assert!(!draws_progress(false, true));
    }

    #[test]
    fn only_the_literal_dumb_term_is_dumb() {
        use crate::test_support::EnvGuard;
        {
            let _env = EnvGuard::new(&[("TERM", Some("dumb"))]);
            assert!(term_is_dumb());
        }
        {
            let _env = EnvGuard::new(&[("TERM", Some("xterm-256color"))]);
            assert!(!term_is_dumb());
        }
        {
            let _env = EnvGuard::new(&[("TERM", None)]);
            assert!(!term_is_dumb(), "an unset TERM is not a dumb terminal");
        }
    }

    /// The transient line always erases what it drew: each update starts by
    /// returning to column 0 and clearing, and the request's end leaves a clean
    /// line rather than a stale fragment.
    #[test]
    fn updates_redraw_in_place_and_the_end_erases_the_line() {
        struct Buf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let shared = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut line = ProgressLine::new(Buf(std::sync::Arc::clone(&shared)), false);

        line.progress(&Progress::new(ProgressPhase::Reading).with_done(2));
        line.progress(&Progress::new(ProgressPhase::Reading).with_done(3));
        line.finished();

        let written = String::from_utf8(shared.lock().unwrap().clone()).unwrap();
        assert_eq!(
            written, "\rreading… 2 documents\x1b[K\rreading… 3 documents\x1b[K\r\x1b[K",
            "each update redraws in place and the finish erases the line"
        );
        assert!(
            !written.contains('\n'),
            "the progress line never advances the terminal"
        );
    }
}
