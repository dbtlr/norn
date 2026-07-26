//! The in-flight progress line (NRN-512): the CLI's rendering of the owner's
//! `Progress` frames.
//!
//! Rendering lives here and only here (invariant 4) — `norn-client` reports
//! typed observations and this layer decides what a user sees. What it decides:
//!
//! - **stderr, never stdout.** Progress is conversation, not payload; nothing
//!   here can reach the payload stream, so a verb's stdout is exactly what it
//!   would be with no progress at all.
//! - **TTY only.** A piped stderr gets nothing at all, because a transient
//!   redrawn line is meaningless in a log or a `2>&1` capture.
//! - **One line, redrawn in place**, erased when the request ends. A long
//!   `norn apply` reports that it is alive without scrolling the terminal.

use std::io::Write;

use norn_client::ProgressSink;
use norn_wire::{Progress, ProgressPhase};

/// The progress sink for this process, or `None` when stderr is not a terminal
/// — the whole TTY gate, read exactly once here. A `None` sink means the session
/// keeps its discarding default: frames are still consumed (that is what resets
/// the client's silence budget), nothing is drawn.
pub fn stderr_progress_sink() -> Option<Box<dyn ProgressSink>> {
    std::io::IsTerminal::is_terminal(&std::io::stderr())
        .then(|| Box::new(ProgressLine::new(std::io::stderr())) as Box<dyn ProgressSink>)
}

/// A single terminal line redrawn in place.
struct ProgressLine<W> {
    out: W,
}

impl<W: Write> ProgressLine<W> {
    fn new(out: W) -> Self {
        Self { out }
    }
}

impl<W: Write + Send> ProgressSink for ProgressLine<W> {
    fn progress(&mut self, progress: &Progress) {
        // `\r` returns to column 0 and `\x1b[K` erases what the previous, longer
        // update left behind. Both are safe unconditionally: this sink is only
        // ever constructed for a terminal.
        //
        // Write errors are DISCARDED, deliberately and uniquely here. Progress
        // is advisory: a command must not fail, or change its exit code, because
        // a decoration could not be drawn. Every payload write still goes
        // through `render_outcome`'s one IO-error policy.
        let _ = write!(self.out, "\r{}\x1b[K", progress_text(progress));
        let _ = self.out.flush();
    }

    fn finished(&mut self) {
        let _ = write!(self.out, "\r\x1b[K");
        let _ = self.out.flush();
    }
}

/// The line's text for one observation. A pure function of the typed frame — no
/// message from the owner is ever echoed (invariant 2), so the wording is this
/// layer's and the phase tag is the only thing crossing the wire.
fn progress_text(progress: &Progress) -> String {
    let (verb, unit) = match progress.phase {
        ProgressPhase::Warming => ("warming", "documents"),
        ProgressPhase::Reading => ("reading", "documents"),
        ProgressPhase::Applying => ("applying", "operations"),
    };
    match (progress.done, progress.total) {
        (Some(done), Some(total)) => format!("{verb}… {done}/{total} {unit}"),
        (Some(done), None) => format!("{verb}… {done} {unit}"),
        (None, Some(total)) => format!("{verb}… {total} {unit} planned"),
        (None, None) => format!("{verb}…"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(phase: ProgressPhase, done: Option<u64>, total: Option<u64>) -> String {
        progress_text(&Progress { phase, done, total })
    }

    #[test]
    fn every_milestone_combination_reads_as_a_sentence() {
        assert_eq!(text(ProgressPhase::Warming, None, None), "warming…");
        assert_eq!(
            text(ProgressPhase::Warming, Some(431), None),
            "warming… 431 documents"
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
        let mut line = ProgressLine::new(Buf(std::sync::Arc::clone(&shared)));

        line.progress(&Progress::new(ProgressPhase::Warming).with_done(2));
        line.progress(&Progress::new(ProgressPhase::Warming).with_done(3));
        line.finished();

        let written = String::from_utf8(shared.lock().unwrap().clone()).unwrap();
        assert_eq!(
            written, "\rwarming… 2 documents\x1b[K\rwarming… 3 documents\x1b[K\r\x1b[K",
            "each update redraws in place and the finish erases the line"
        );
        assert!(
            !written.contains('\n'),
            "the progress line never advances the terminal"
        );
    }
}
