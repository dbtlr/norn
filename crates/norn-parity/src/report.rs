//! Deterministic rendering of a run's results to stdout text — the one home
//! for both the comparison report and the consistency report, each paired
//! with the exit code it maps to. Row order follows `crate::cases::suites()`
//! declaration order (the run order) — never a `HashMap`, so nothing here
//! can reorder between runs.

use crate::consistency::Finding;
use crate::poststate::PostStateDiff;
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
    render_post_state(&mut out, report);
    out
}

/// Append a legible post-state section for every mutating case whose two
/// vault trees differed: which relative paths differ, and — for a shared path
/// — a concise byte-length summary, never a full body dump. Nothing is emitted
/// when no mutating case diverged on its tree (the common case, and every case
/// today, since none mutate yet).
fn render_post_state(out: &mut String, report: &RunReport) {
    let diverged: Vec<(&str, &PostStateDiff)> = report
        .outcomes
        .iter()
        .filter_map(|o| o.post_state.as_ref().map(|d| (o.case_id, d)))
        .collect();
    if diverged.is_empty() {
        return;
    }
    out.push_str("post-state divergences (vault tree differs after mutation):\n");
    for (case_id, diff) in diverged {
        out.push_str(&format!("  {case_id}:\n"));
        if !diff.only_in_oracle.is_empty() {
            out.push_str(&format!(
                "    only in oracle: {}\n",
                diff.only_in_oracle.join(", ")
            ));
        }
        if !diff.only_in_candidate.is_empty() {
            out.push_str(&format!(
                "    only in candidate: {}\n",
                diff.only_in_candidate.join(", ")
            ));
        }
        for delta in &diff.content_differs {
            out.push_str(&format!(
                "    content differs: {} (oracle {} bytes, candidate {} bytes)\n",
                delta.path, delta.oracle_len, delta.candidate_len
            ));
        }
    }
}

/// Render the oracle self-consistency result and the exit code it maps to:
/// exit 0 with a clean line when nothing disagrees; exit 1 with one line per
/// disagreement (each a candidate divergence-ledger entry, ADR 0018).
pub fn render_consistency(findings: &[Finding]) -> (String, u8) {
    if findings.is_empty() {
        return ("consistency: 0 disagreements\n".to_string(), 0);
    }
    let mut out = String::new();
    for f in findings {
        out.push_str(&format!(
            "disagreement [{}] fixture={}: {}\n",
            f.check, f.fixture, f.message
        ));
    }
    (out, 1)
}
