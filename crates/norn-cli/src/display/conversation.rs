//! The stderr-only conversation channel (NRN-370).
//!
//! stdout is payload-only, structurally: a rendered report reaches stdout through
//! the [`Sink`](crate::display::Sink), and everything else a verb has to say — the
//! report annotations (`note:` / `warning:` truncation and
//! `--col` lines) and status lines — goes here, on stderr. A [`Conversation`]
//! wraps only the stderr sink, so a renderer that holds one structurally cannot
//! write to the payload stream.
//!
//! This is a distinct stream from the diagnostic path: user *errors* still render
//! through [`Presenter::present_diagnostic`](crate::display::Presenter) with the
//! `norn:` prefix. The conversation carries the annotation format, which is
//! deliberately NOT `norn:`-prefixed (ADR 0020).

use std::io::{self, Write};

use norn_wire::{ApplyReport, Note, Severity};

use super::PROGRAM;

/// The closed stderr-annotation prefix an annotation renders with. The
/// conversation channel speaks exactly three prefixes — `note:` (informational,
/// exit 0), `warning:` (a non-fatal issue, exit 0), and `error:` (a fatal
/// condition, a non-zero exit) — so a consumer branches on one stable prefix set
/// (ADR 0021). A report [`Note`] maps its [`Severity`] onto `warning:` / `error:`
/// here; `note:` is reserved for the CLI's own informational annotations (e.g. a
/// truncation notice), which carry no severity.
pub(crate) fn severity_prefix(severity: Severity) -> &'static str {
    match severity {
        Severity::Warning => "warning",
        Severity::Error => "error",
    }
}

/// The stderr-only writer a renderer emits report annotations and status lines
/// through. Borrows the presenter's stderr sink for the duration of one render.
pub struct Conversation<'a> {
    err: &'a mut dyn Write,
}

impl<'a> Conversation<'a> {
    /// Wrap a stderr sink.
    pub fn new(err: &'a mut dyn Write) -> Self {
        Self { err }
    }

    /// Write one annotation/status line verbatim, plus a newline. The
    /// annotation lines (`note:` / `warning:` / a resolution note) carry their
    /// own prefix in the message — the conversation adds none.
    pub fn line(&mut self, text: &str) -> io::Result<()> {
        writeln!(self.err, "{text}")
    }

    /// The raw stderr sink, for the shared `warn_*` helpers that take a
    /// `&mut dyn Write` (`projection::warn_col_ignored` and friends).
    pub fn writer(&mut self) -> &mut dyn Write {
        self.err
    }

    /// One informational `note: <msg>` annotation (exit 0) — the CLI's own
    /// non-severity heads-up, e.g. a truncation notice.
    pub fn note(&mut self, msg: &str) -> io::Result<()> {
        writeln!(self.err, "note: {msg}")
    }

    /// One `warning: <msg>` annotation — a non-fatal issue that never flips the
    /// exit.
    pub fn warning(&mut self, msg: &str) -> io::Result<()> {
        writeln!(self.err, "warning: {msg}")
    }

    /// The trust-posture operator advisory for a degraded telemetry sink
    /// (NRN-400 review): a CONFIRMED apply whose durable events dir/file could
    /// not be opened, or whose write mid-failed, still mints and prints a real
    /// `trace_id` — the executor never blocks a write on telemetry — but that
    /// id correlates to no durable audit line, so the operator gets nothing
    /// back from a later `norn audit --trace <id>`. This is the ONE signal
    /// they get on the CLI surface (the owner log carries the underlying IO
    /// error); shares the closed `warning:` vocabulary rather than inventing a
    /// new prefix. Call only when the report's `telemetry_degraded` flag is
    /// set — a no-op call site would print a false alarm.
    pub fn telemetry_degraded_warning(&mut self) -> io::Result<()> {
        writeln!(
            self.err,
            "warning: audit trail not persisted for this apply (durable write failed)"
        )
    }

    /// One `error: <msg>` annotation — a fatal condition the verb reports before
    /// exiting non-zero.
    pub fn error(&mut self, msg: &str) -> io::Result<()> {
        writeln!(self.err, "error: {msg}")
    }

    /// Render one report [`Note`] as a POSIX-shaped stderr line, deriving the
    /// prefix from the note's typed [`Severity`] (never from its message text).
    /// A read verb's `error`-severity note is the exit-1 signal; the exit
    /// decision reads [`Note::is_error`], this call only renders.
    pub fn report_note(&mut self, note: &Note) -> io::Result<()> {
        writeln!(
            self.err,
            "{}: {}",
            severity_prefix(note.severity),
            note.message
        )
    }

    /// One `norn: <msg>` diagnostic headline on stderr — the prefixed form for
    /// the handful of renderer-internal status lines (`vault list`'s empty and
    /// serialize-failure cases) that carry the program prefix rather than the
    /// bare annotation shape. Mirrors
    /// [`Presenter::diagnostic`](super::Presenter::diagnostic)'s bytes so the
    /// stderr headline can never drift between the two entry points.
    pub fn diagnostic(&mut self, msg: &str) {
        let _ = writeln!(self.err, "{PROGRAM}: {msg}");
    }

    /// The cascade-failure warnings (real FS errors that left backlinks
    /// dangling) for the cascade verbs (`move` / `delete` / `rewrite-wikilink` /
    /// `apply`), on stderr. A no-op
    /// on the common (failure-free) path.
    pub fn cascade_failure_warnings(&mut self, report: &ApplyReport) {
        for op in &report.operations {
            let Some(cascade) = op.cascade.as_ref() else {
                continue;
            };
            if cascade.failed == 0 {
                continue;
            }
            let _ = writeln!(
                self.err,
                "warning: {} backlink{} could not be rewritten after retries and now dangle{}:",
                cascade.failed,
                if cascade.failed == 1 { "" } else { "s" },
                if cascade.failed == 1 { "s" } else { "" },
            );
            for f in &cascade.failures {
                let _ = match &f.detail {
                    Some(d) => writeln!(
                        self.err,
                        "  {}: {} → {} ({}: {})",
                        f.file, f.from, f.to, f.reason, d
                    ),
                    None => writeln!(
                        self.err,
                        "  {}: {} → {} ({})",
                        f.file, f.from, f.to, f.reason
                    ),
                };
            }
            let _ = writeln!(
                self.err,
                "  fix manually, or run `norn validate` to list dangling links."
            );
        }
    }
}
