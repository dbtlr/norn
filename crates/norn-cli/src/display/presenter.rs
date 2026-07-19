//! The writer abstraction command modules present through.
//!
//! A [`Presenter`] owns the stdout and stderr sinks so a command never touches
//! the process streams directly — the bin builds one over the real streams
//! ([`Presenter::stdio`]); tests build one over in-memory buffers
//! ([`Presenter::new`]) and assert on the exact bytes without spawning. Report
//! rendering methods land on this type as the verbs port; today it carries the
//! stderr `norn:` convention and the uniform not-yet-ported outcome.

use std::io::{self, Write};

use super::Diagnostic;

/// The program-name prefix on every stderr diagnostic headline.
pub const PROGRAM: &str = "norn";

/// The prefix on every stderr soft-landing hint line (NRN-361).
pub const HINT: &str = "hint";

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

    /// Present a structured [`Diagnostic`] on stderr, and ONLY stderr — the
    /// single rendering path for every user-error site (NRN-361). Line 1 is the
    /// prefixed headline (`norn: <message>`, stable greppable shape); each
    /// following line is a soft-landing `hint: <hint>`. Hints are
    /// tty-independent (agents read pipes), and nothing here ever touches
    /// stdout — the payload stream stays clean in every format.
    pub fn present_diagnostic(&mut self, diag: &Diagnostic) {
        let _ = writeln!(self.err, "{PROGRAM}: {}", diag.message());
        for hint in diag.hints() {
            let _ = writeln!(self.err, "{HINT}: {hint}");
        }
    }

    /// Write one `norn: <msg>` diagnostic headline to stderr — the headline-only
    /// convenience, routed through the single [`present_diagnostic`] path so it
    /// can never drift from the structured form.
    ///
    /// [`present_diagnostic`]: Self::present_diagnostic
    pub fn diagnostic(&mut self, msg: &str) {
        self.present_diagnostic(&Diagnostic::new(msg));
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
    fn present_diagnostic_renders_headline_then_hints_on_stderr_only() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut p = Presenter::new(&mut out, &mut err);
            let diag = Diagnostic::new("no vault named \"atlas\" is registered")
                .with_hint("run `norn vault list` to see registered vault names")
                .with_hint("register it with `norn vault register`");
            p.present_diagnostic(&diag);
        }
        assert!(out.is_empty(), "a diagnostic must never touch stdout");
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "norn: no vault named \"atlas\" is registered\n\
             hint: run `norn vault list` to see registered vault names\n\
             hint: register it with `norn vault register`\n"
        );
    }

    #[test]
    fn diagnostic_headline_only_matches_the_structured_form() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut p = Presenter::new(&mut out, &mut err);
            p.diagnostic("bad predicate");
        }
        assert!(out.is_empty());
        assert_eq!(String::from_utf8(err).unwrap(), "norn: bad predicate\n");
    }

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
