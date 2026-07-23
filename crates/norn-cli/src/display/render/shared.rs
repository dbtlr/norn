//! Cross-verb render helpers (NRN-409): the handful of pieces more than one
//! verb module reaches for. Single-verb helpers stay put in their own module.

use std::io::{self, Write};

use norn_wire::{ApplyOutcome, ApplyReport, FindReport, MutationOutcome, MutationWarning};
use serde_json::Value;

use crate::display::conversation::Conversation;
use crate::display::{serde_label, Format, EXIT_OK, EXIT_USAGE};

/// `""` for a count of 1, `"s"` otherwise.
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
/// every other JSON value prints its compact JSON (`3`, `["a","b"]`, `null`).
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
        MutationOutcome::Applied | MutationOutcome::Forecast => EXIT_OK,
    }
}

/// Render the non-fatal mutation warnings (count + the first three messages, with
/// a `… (N more)` tail) on the stderr conversation.
/// The mutation warning's records short form, computed per `code` from the
/// unified `{ code, field, message }` envelope (see `norn_wire::MutationWarning`).
/// Kinds whose records line differs from the message (`unknown-field`,
/// `force-bypass`, `title-ignored`) are rebuilt from `code` + `field`; the rest
/// print their `message` verbatim (e.g. the wikilink warnings).
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

/// Render a refused cascade `ApplyReport`. `--format json` carries the FULL
/// report envelope (`outcome: refused` plus the failed op holding the coded
/// `{code,message,path?}` error) — the one serializer policy every mutation
/// verb's JSON refusal now follows, so a consumer parses ONE shape on every
/// path (applied / forecast / refused) and never a bare `{code,message}`
/// fragment stripped of the envelope. Records prints `error: <message>` on
/// stderr. Exit 2 either way.
pub(super) fn render_apply_refusal(
    report: &ApplyReport,
    format: Format,
    out: &mut dyn Write,
    conv: &mut Conversation,
) -> i32 {
    let json = matches!(format, Format::Json);
    if json {
        let result: std::io::Result<i32> = (|| {
            write_report_json(out, report)?;
            Ok(report.exit_code())
        })();
        crate::display::emit::render_outcome(result, conv.writer())
    } else {
        let error = report
            .operations
            .iter()
            .find_map(|o| o.error.as_ref())
            .or_else(|| report.preconditions.iter().find_map(|p| p.error.as_ref()));
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

/// The truncation note both `paths` and `jsonl` emit on stderr.
pub(super) fn truncation_note(report: &FindReport) -> String {
    format!(
        "note: showing {} of {} (--no-limit for all)",
        report.returned, report.total
    )
}

/// The ONE mutation-report JSON serializer policy (NRN-408): a report's
/// pretty-printed serialization in serde struct-field order plus exactly one
/// trailing newline. Every mutation verb's `--format json` — `set` / `new` /
/// `edit` and the cascade verbs `apply` / `move` / `delete` /
/// `rewrite-wikilink` — writes through here on EVERY outcome path
/// (applied / forecast / refused), so the compactness, key ordering, and
/// trailing-newline rules are defined in exactly one place. Generic over the
/// report type because the compact `SetReport` / `NewReport` / `EditReport`
/// and the nested `ApplyReport` all serialize under the same rule.
pub(super) fn write_report_json<T: serde::Serialize>(
    out: &mut dyn Write,
    report: &T,
) -> io::Result<()> {
    writeln!(out, "{}", serde_json::to_string_pretty(report)?)
}

/// Write an `ApplyReport`'s pretty JSON serialization to the `--out` file path —
/// the write shared by the cascade verbs that accept `--out` (`apply`,
/// `rewrite-wikilink`); the caller silences stdout on this path. Routes through
/// `write_report_json` so the file projection and the stdout projection are the
/// same one-policy serialization, never two independent implementations.
pub(super) fn write_report_to_out_file(path: &str, report: &ApplyReport) -> io::Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    write_report_json(&mut buf, report)?;
    std::fs::write(path, buf)
}

/// The status-label vocabulary an `ApplyReport`'s outcome maps to in a TTY
/// headline (`apply <label>`, `move-folder <label>`): a dry-run forecast reads
/// `dry-run`, a clean apply `applied`, a runtime op failure `failed`, a
/// preflight refusal `refused`. The one place a records headline decides
/// between preview/success/failure words, so every verb that prints one asks
/// this rather than re-deriving it from `dry_run` alone (which can't tell a
/// real failure from a success).
pub(super) fn apply_status_label(report: &ApplyReport) -> &'static str {
    match report.outcome {
        ApplyOutcome::Applied if report.dry_run => "dry-run",
        ApplyOutcome::Applied => "applied",
        ApplyOutcome::Failed => "failed",
        ApplyOutcome::Refused => "refused",
        ApplyOutcome::Rebased => "rebased",
    }
}

/// The counts + preconditions + per-op + warnings body of the generic
/// apply-report records block, independent of
/// the headline: shared by `apply`'s own full report render and by
/// [`render_cascade_failed`], the truthful fallback the other cascade verbs
/// render when their own report's outcome is `failed`.
pub(super) fn render_apply_report_body(
    out: &mut dyn Write,
    report: &ApplyReport,
) -> io::Result<()> {
    writeln!(
        out,
        "  applied: {}  skipped: {}  failed: {}  remaining: {}",
        report.applied, report.skipped, report.failed, report.remaining
    )?;
    if !report.preconditions.is_empty() {
        writeln!(out, "preconditions:")?;
        for precondition in &report.preconditions {
            let status = serde_label(&precondition.status);
            writeln!(out, "  [{status}] {}", precondition.id)?;
            if let Some(error) = &precondition.error {
                writeln!(out, "    {}: {}", error.code, error.message)?;
            }
        }
    }
    for op in &report.operations {
        let status = serde_label(&op.status);
        writeln!(out, "  [{status}] {}", op.summary)?;
    }
    if !report.warnings.is_empty() {
        writeln!(out, "warnings:")?;
        for w in &report.warnings {
            writeln!(out, "  {}: {}", w.code, w.message)?;
        }
    }
    Ok(())
}

/// A FAILED cascade report's truthful records headline (`{verb} failed`) plus
/// the shared apply-report body — the fallback `move`/`delete`/
/// `rewrite-wikilink` render instead of their own success/preview wording when
/// the report's outcome is `failed`, so a real runtime failure never reads like
/// an applied or forecast run.
pub(super) fn render_cascade_failed(
    out: &mut dyn Write,
    verb: &str,
    report: &ApplyReport,
) -> io::Result<()> {
    writeln!(out, "{verb} failed")?;
    render_apply_report_body(out, report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f5_warning_short_rebuilds_the_records_labels_per_code() {
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
