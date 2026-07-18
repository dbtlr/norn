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
