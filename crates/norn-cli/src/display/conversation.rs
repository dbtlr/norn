//! The stderr-only conversation channel (NRN-370).
//!
//! stdout is payload-only, structurally: a rendered report reaches stdout through
//! the [`Sink`](crate::display::Sink), and everything else a verb has to say — the
//! pinned oracle-parity report annotations (`note:` / `warning:` truncation and
//! `--col` lines) and status lines — goes here, on stderr. A [`Conversation`]
//! wraps only the stderr sink, so a renderer that holds one structurally cannot
//! write to the payload stream.
//!
//! This is a distinct stream from the diagnostic path: user *errors* still render
//! through [`Presenter::present_diagnostic`](crate::display::Presenter) with the
//! `norn:` prefix. The conversation carries the oracle's byte-matched annotation
//! format, which is deliberately NOT `norn:`-prefixed (ADR 0020).

use std::io::{self, Write};

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

    /// Write one annotation/status line verbatim, plus a newline. The pinned
    /// oracle-parity lines (`note:` / `warning:` / a resolution note) carry their
    /// own prefix in the message — the conversation adds none.
    pub fn line(&mut self, text: &str) -> io::Result<()> {
        writeln!(self.err, "{text}")
    }

    /// The raw stderr sink, for the shared `warn_*` helpers that take a
    /// `&mut dyn Write` (`projection::warn_col_ignored` and friends).
    pub fn writer(&mut self) -> &mut dyn Write {
        self.err
    }
}
