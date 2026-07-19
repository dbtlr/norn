//! The structured diagnostic the CLI presents on stderr (NRN-361).
//!
//! Errors educate, never stop-sign. A [`Diagnostic`] carries the concise,
//! prefixed headline plus zero or more soft-landing `hint:` lines, and is
//! rendered by exactly ONE presenter path
//! ([`Presenter::present_diagnostic`](crate::display::Presenter::present_diagnostic)).
//! Per-verb code builds a `Diagnostic` and hands it over; it never formats its
//! own `norn:` / `hint:` stderr lines. This is the enforcement layer — the
//! prefix, the hint prefix, and the stderr-only rule all live in one place, so
//! a new error site cannot drift the shape.
//!
//! # The contract
//!
//! - **stdout is payload only.** A diagnostic never touches stdout in any
//!   format (including `--format json`, where the payload is the JSON body).
//! - **stderr is the conversation channel**, rustc-style: line 1 is the
//!   headline (`norn: <what went wrong>` — stable, greppable shape), and each
//!   following `hint:` line is the soft landing (a did-you-mean where a
//!   near-miss exists, or a next-step where a diagnosis path exists).
//! - **hints are tty-independent.** They are for agents too, so they are
//!   emitted whether stderr is a terminal or a pipe.
//! - **educate when possible, never noise.** A headline alone is a complete
//!   diagnostic; hints are attached only where a genuinely useful one exists.

/// A user-facing diagnostic: one headline plus optional soft-landing hints.
///
/// The headline is rendered as `norn: <message>`; each hint as `hint: <hint>`.
/// Constructed at the error site and handed to the presenter — the rendering
/// (prefixes, stream, ordering) is the presenter's, not the call site's.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Diagnostic {
    message: String,
    hints: Vec<String>,
}

impl Diagnostic {
    /// A headline-only diagnostic — the complete, no-hint form.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            hints: Vec::new(),
        }
    }

    /// Attach one soft-landing hint, builder-style.
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hints.push(hint.into());
        self
    }

    /// Attach a run of soft-landing hints, builder-style. Empty input is a
    /// no-op, so folding the wire-carried hint list in is always safe.
    #[must_use]
    pub fn with_hints<I, S>(mut self, hints: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.hints.extend(hints.into_iter().map(Into::into));
        self
    }

    /// The concise headline (rendered after the `norn:` prefix).
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The soft-landing hints, in attach order (each rendered `hint: <hint>`).
    pub fn hints(&self) -> &[String] {
        &self.hints
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headline_only_has_no_hints() {
        let d = Diagnostic::new("something went wrong");
        assert_eq!(d.message(), "something went wrong");
        assert!(d.hints().is_empty());
    }

    #[test]
    fn hints_accumulate_in_attach_order() {
        let d = Diagnostic::new("headline")
            .with_hint("first")
            .with_hint("second");
        assert_eq!(d.hints(), ["first".to_string(), "second".to_string()]);
    }

    #[test]
    fn with_hints_folds_an_iterator_and_empty_is_a_noop() {
        let d = Diagnostic::new("headline").with_hints(Vec::<String>::new());
        assert!(d.hints().is_empty());
        let d = d.with_hints(vec!["a".to_string(), "b".to_string()]);
        assert_eq!(d.hints(), ["a".to_string(), "b".to_string()]);
    }
}
