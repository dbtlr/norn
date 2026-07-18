//! Deterministic rendering of a [`crate::run::RunReport`] to stdout text.
//! Row order follows `crate::cases::suites()` declaration order (the run
//! order) — never a `HashMap`, so nothing here can reorder between runs.

use crate::run::{Mode, RunReport};
use crate::verdict::Verdict;

/// Render the verdict table + summary line for `report`, run under `mode`.
///
/// Phase 0 special case: a gated run with zero selected cases (every suite
/// is `ported: false` until phases 1-3 flip them) prints the single
/// burn-down line the ADR calls for instead of an empty table.
pub fn render(report: &RunReport, mode: Mode) -> String {
    if matches!(mode, Mode::Gated) && report.outcomes.is_empty() {
        return "0 suites gated\n".to_string();
    }

    let mut out = String::new();
    let id_width = report
        .outcomes
        .iter()
        .map(|o| o.case_id.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let suite_width = report
        .outcomes
        .iter()
        .map(|o| o.suite_name.len())
        .max()
        .unwrap_or(5)
        .max(5);

    out.push_str(&format!(
        "{:id_width$}  {:suite_width$}  VERDICT   ENTRY\n",
        "CASE", "SUITE"
    ));
    let mut match_count = 0usize;
    let mut diverged_count = 0usize;
    let mut drift_count = 0usize;
    for outcome in &report.outcomes {
        let entry_col = match &outcome.verdict {
            Verdict::Diverged { entry_id } => entry_id.as_str(),
            _ => "-",
        };
        match outcome.verdict {
            Verdict::Match => match_count += 1,
            Verdict::Diverged { .. } => diverged_count += 1,
            Verdict::Drift => drift_count += 1,
        }
        out.push_str(&format!(
            "{:id_width$}  {:suite_width$}  {:<8}  {}\n",
            outcome.case_id,
            outcome.suite_name,
            outcome.verdict.label(),
            entry_col
        ));
    }

    out.push_str(&format!(
        "{} cases: {match_count} match, {diverged_count} diverged, {drift_count} drift, {} stale entries\n",
        report.outcomes.len(),
        report.stale_entries.len()
    ));
    if !report.stale_entries.is_empty() {
        out.push_str(&format!(
            "stale entries (all cited cases currently match — entries cannot rot): {}\n",
            report.stale_entries.join(", ")
        ));
    }
    out
}
