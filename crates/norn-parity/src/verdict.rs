//! The three verdicts (ADR 0018) and the comparator. There is no fourth
//! state — a case that cannot run at all (missing binary, generation
//! failure, a process killed by signal) never reaches this module; it is a
//! runner error (see `crate::run`).

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Normalized stdout, stderr, and exit code all match.
    Match,
    /// They differ, and the difference is covered by exactly one ledger
    /// entry — passes, citing the entry.
    Diverged { entry_id: String },
    /// They differ and no ledger entry covers it — fails.
    Drift,
}

impl Verdict {
    pub fn label(&self) -> &'static str {
        match self {
            Verdict::Match => "match",
            Verdict::Diverged { .. } => "diverged",
            Verdict::Drift => "drift",
        }
    }
}

/// `true` when both normalized outputs are byte-for-byte identical across
/// stdout, stderr, and exit code.
pub fn outputs_match(
    a: &crate::normalize::NormalizedOutput,
    b: &crate::normalize::NormalizedOutput,
) -> bool {
    a.stdout == b.stdout && a.stderr == b.stderr && a.exit_code == b.exit_code
}

/// The single classification rule shared by every comparison the runner
/// makes — the one production home for "match/diverged/drift", so `run::run`
/// and its end-to-end tests exercise identical logic:
///
/// - outputs match -> [`Verdict::Match`];
/// - they differ under self-check (oracle vs. itself, ledger not consulted)
///   -> [`Verdict::Drift`] (a self-comparison must never diverge);
/// - they differ with a covering ledger entry -> [`Verdict::Diverged`]
///   citing it;
/// - they differ with no entry -> [`Verdict::Drift`].
pub fn classify(outputs_match: bool, self_check: bool, ledger_entry_id: Option<&str>) -> Verdict {
    if outputs_match {
        Verdict::Match
    } else if self_check {
        Verdict::Drift
    } else {
        match ledger_entry_id {
            Some(id) => Verdict::Diverged {
                entry_id: id.to_string(),
            },
            None => Verdict::Drift,
        }
    }
}
