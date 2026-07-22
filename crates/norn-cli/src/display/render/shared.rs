//! Cross-verb render helpers (NRN-409): the handful of pieces more than one
//! verb module reaches for. Single-verb helpers stay put in their own module.

use std::io::Write;

use norn_wire::{ApplyReport, FindReport, MutationOutcome, MutationWarning};
use serde_json::Value;

use crate::display::conversation::Conversation;
use crate::display::{EXIT_OK, EXIT_USAGE};

/// `""` for a count of 1, `"s"` otherwise — the donor pluralization.
pub(super) fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Singular/plural noun selector for the delete summary.
pub(super) fn noun(n: usize, singular: &'static str, plural: &'static str) -> &'static str {
    if n == 1 {
        singular
    } else {
        plural
    }
}

/// Render a value for a change line: a bare string prints unquoted (`draft`),
/// every other JSON value prints its compact JSON (`3`, `["a","b"]`, `null`) —
/// the donor `value_repr`.
pub(super) fn value_repr(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The exit code a mutation report implies: a clean pre-write decline
/// (`outcome = refused`) is exit 2 (the refusal is authoritative, nothing
/// happened); a forecast or applied report is exit 0.
pub(super) fn mutation_exit(outcome: MutationOutcome) -> i32 {
    match outcome {
        MutationOutcome::Refused => EXIT_USAGE,
        MutationOutcome::Applied => EXIT_OK,
    }
}

/// Render the non-fatal mutation warnings (count + the first three messages, with
/// a `… (N more)` tail) — the donor truncation, on the stderr conversation.
/// The mutation warning's records short form — the donor `warning_label`
/// vocabulary, computed per `code` from the unified `{ code, field, message }`
/// envelope (the JSON shape is a deliberate divergence; see
/// `norn_wire::MutationWarning`). Kinds whose records line differs from the
/// message (`unknown-field`, `force-bypass`, `title-ignored`) are rebuilt from
/// `code` + `field`; the rest print their `message` verbatim (it already equals
/// the donor label — e.g. the wikilink warnings).
pub(super) fn warning_short(w: &MutationWarning) -> String {
    let field = w.field.as_deref().unwrap_or("");
    match w.code.as_str() {
        "unknown-field" => format!("unknown field: {field}"),
        "force-bypass" => format!("--force bypass: {field}"),
        "title-ignored" => format!("title-ignored: {}", w.message),
        _ => w.message.clone(),
    }
}

/// The exit code an `ApplyReport` implies, via its own outcome→exit mapping
/// (applied/dry-run → 0, partial-failure → 1, refused → 2).
pub(super) fn apply_report_exit(report: &ApplyReport) -> i32 {
    report.exit_code()
}

/// Render a refused cascade `ApplyReport` (the donor `emit_refusal`): the pretty
/// coded error envelope on stdout for `--format json`, else `error: <message>` on
/// stderr. Exit 2.
pub(super) fn render_apply_refusal(
    report: &ApplyReport,
    json: bool,
    out: &mut dyn Write,
    conv: &mut Conversation,
) -> i32 {
    let error = report
        .operations
        .iter()
        .find_map(|o| o.error.as_ref())
        .or_else(|| report.preconditions.iter().find_map(|p| p.error.as_ref()));
    if json {
        let result: std::io::Result<i32> = (|| {
            match error {
                Some(e) => writeln!(out, "{}", serde_json::to_string_pretty(e)?)?,
                None => writeln!(out, "{{}}")?,
            }
            Ok(EXIT_USAGE)
        })();
        crate::display::emit::render_outcome(result, conv.writer())
    } else {
        let msg = error
            .map(|e| e.message.clone())
            .unwrap_or_else(|| "refused".to_string());
        let result: std::io::Result<i32> = (|| {
            conv.line(&format!("error: {msg}"))?;
            Ok(EXIT_USAGE)
        })();
        crate::display::emit::render_outcome(result, conv.writer())
    }
}

/// The truncation note both `paths` and `jsonl` emit on stderr (donor parity).
pub(super) fn truncation_note(report: &FindReport) -> String {
    format!(
        "note: showing {} of {} (--no-limit for all)",
        report.returned, report.total
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f5_warning_short_rebuilds_the_donor_records_labels_per_code() {
        let uf = MutationWarning {
            code: "unknown-field".into(),
            field: Some("status".into()),
            message: "field 'status' not declared in schema".into(),
        };
        assert_eq!(warning_short(&uf), "unknown field: status");

        let ti = MutationWarning {
            code: "title-ignored".into(),
            field: None,
            message: "--title 'X' has no effect with an explicit path".into(),
        };
        assert_eq!(
            warning_short(&ti),
            "title-ignored: --title 'X' has no effect with an explicit path"
        );

        let fb = MutationWarning {
            code: "force-bypass".into(),
            field: Some("status".into()),
            message: "--force bypassed type validation for 'status'".into(),
        };
        assert_eq!(warning_short(&fb), "--force bypass: status");
    }
}
