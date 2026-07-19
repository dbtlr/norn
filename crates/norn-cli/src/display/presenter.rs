//! The writer abstraction command modules present through.
//!
//! A [`Presenter`] owns the stdout and stderr sinks so a command never touches
//! the process streams directly — the bin builds one over the real streams
//! ([`Presenter::stdio`]); tests build one over in-memory buffers
//! ([`Presenter::new`]) and assert on the exact bytes without spawning. Report
//! rendering methods land on this type as the verbs port; today it carries the
//! stderr `norn:` convention and the uniform not-yet-ported outcome.

use std::io::{self, Write};

/// The program-name prefix on every stderr diagnostic line.
pub const PROGRAM: &str = "norn";

/// The stdout / stderr sinks a command presents through.
pub struct Presenter<O: Write, E: Write> {
    out: O,
    err: E,
}

impl Presenter<io::Stdout, io::Stderr> {
    /// A presenter over the process's real stdout and stderr.
    pub fn stdio() -> Self {
        Self {
            out: io::stdout(),
            err: io::stderr(),
        }
    }
}

impl<O: Write, E: Write> Presenter<O, E> {
    /// A presenter over arbitrary sinks — the test seam.
    pub fn new(out: O, err: E) -> Self {
        Self { out, err }
    }

    /// The stdout sink command modules render Reports into. Report-rendering
    /// helpers land here per-verb (the `src/output/primitives` port); today the
    /// raw sink is the whole seam.
    pub fn out(&mut self) -> &mut O {
        &mut self.out
    }

    /// Both sinks at once — for renderers that write records/JSON to stdout and
    /// truncation notes / `--col` warnings to stderr in one pass.
    pub fn streams(&mut self) -> (&mut O, &mut E) {
        (&mut self.out, &mut self.err)
    }

    /// Write one `norn: <msg>` diagnostic line to stderr.
    pub fn diagnostic(&mut self, msg: &str) {
        let _ = writeln!(self.err, "{PROGRAM}: {msg}");
    }

    /// The uniform not-yet-ported outcome: one stderr line naming the command,
    /// and the operational exit code. One helper, so every unported command
    /// emits a byte-identical line.
    pub fn not_yet_ported(&mut self, command: &str) -> i32 {
        self.diagnostic(&format!(
            "`{command}` is not yet ported in this build (rewrite in progress; see ADR 0018)"
        ));
        super::EXIT_OPERATIONAL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_yet_ported_writes_uniform_line_and_returns_operational() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut p = Presenter::new(&mut out, &mut err);
            p.not_yet_ported("get")
        };
        assert_eq!(code, super::super::EXIT_OPERATIONAL);
        assert!(out.is_empty(), "stdout must stay empty");
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "norn: `get` is not yet ported in this build (rewrite in progress; see ADR 0018)\n"
        );
    }
}
