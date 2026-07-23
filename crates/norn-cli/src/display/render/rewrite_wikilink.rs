//! `rewrite-wikilink` (one of the cascade verbs) (NRN-409).
//!
//! Renders the shared `ApplyReport` the donor emits (byte-faithful to
//! `retired/src/rewrite_wikilink/route.rs`). `--format json` is the report's
//! PRETTY serialization (with a trailing newline); records is the donor's
//! human summary. A refused report renders the coded `{code,message,path?}`
//! envelope (json, pretty) or `error: <msg>` (records) and exits 2. Cascade
//! failures (real FS errors) surface on stderr before the summary.

use std::io::{self, Write};

use norn_wire::{ApplyOutcome, ApplyReport};

use crate::display::conversation::Conversation;
use crate::display::emit::render_outcome;
use crate::display::output::RewriteWikilinkView;
use crate::display::sink::Sink;

use super::shared::{
    apply_report_exit, render_apply_refusal, render_cascade_failed, write_report_json,
    write_report_to_out_file,
};

pub(crate) fn render_rewrite_wikilink(
    view: RewriteWikilinkView,
    sink: &mut Sink<'_>,
    conv: &mut Conversation<'_>,
) -> i32 {
    let report = &view.report;

    if report.outcome == ApplyOutcome::Refused {
        // `--out` is a write-to-file projection of the report; a refusal never
        // reaches it (the donor refuses before rendering), so the envelope path
        // is unconditional here.
        return render_apply_refusal(report, view.json, sink.writer(), conv);
    }

    conv.cascade_failure_warnings(report);
    let exit = apply_report_exit(report);

    // `--out`: write the (always-JSON, pretty) report to the file, silence stdout.
    if let Some(out_path) = &view.out {
        let result: io::Result<i32> = (|| {
            write_report_to_out_file(out_path, report)?;
            Ok(exit)
        })();
        return render_outcome(result, conv.writer());
    }

    if view.json {
        let result: io::Result<i32> = (|| {
            write_report_json(sink.writer(), report)?;
            Ok(exit)
        })();
        return render_outcome(result, conv.writer());
    }

    let result: io::Result<i32> = (|| {
        render_rewrite_records(sink.writer(), report, &view.old, &view.new)?;
        if !report.dry_run {
            sink.trace_footer(&report.trace_id)?;
        }
        Ok(exit)
    })();
    render_outcome(result, conv.writer())
}

/// Records-format rewrite-wikilink output (donor `rewrite_wikilink::render_records`).
/// A runtime op failure (`outcome = failed`) renders the truthful shared
/// `render_cascade_failed` headline instead of the `would rewrite`/`rewrote`
/// wording below, which only distinguishes forecast from success.
fn render_rewrite_records(
    out: &mut dyn Write,
    report: &ApplyReport,
    old: &str,
    new: &str,
) -> io::Result<()> {
    if report.outcome == ApplyOutcome::Failed {
        return render_cascade_failed(out, "rewrite-wikilink", report);
    }
    let body_count = report
        .operations
        .iter()
        .filter(|o| o.kind == "rewrite_link")
        .count();
    let fm_count = report
        .operations
        .iter()
        .filter(|o| o.kind == "set_frontmatter")
        .count();
    let total = report.operations.len();
    let status = if report.dry_run {
        "would rewrite"
    } else {
        "rewrote"
    };
    writeln!(
        out,
        "{status} [[{old}]] → [[{new}]] in {total} ops ({body_count} body + {fm_count} frontmatter)"
    )?;
    if !report.warnings.is_empty() {
        writeln!(out, "warnings:")?;
        for w in &report.warnings {
            writeln!(out, "  {}: {}", w.code, w.message)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use norn_wire::{ApplyReportOp, OpStatus};

    fn failed_report() -> ApplyReport {
        ApplyReport {
            schema_version: 3,
            trace_id: String::new(),
            plan_hash: String::new(),
            vault_root: "/vault".into(),
            dry_run: false,
            applied: 1,
            skipped: 0,
            failed: 1,
            remaining: 0,
            preconditions: Vec::new(),
            operations: vec![
                ApplyReportOp {
                    op_id: "0".into(),
                    kind: "rewrite_link".into(),
                    status: OpStatus::Applied,
                    from: None,
                    path: Some("a.md".into()),
                    stem: Some("a".into()),
                    summary: "rewrote [[old]] -> [[new]] in a.md".into(),
                    error: None,
                    footnote: None,
                    cascade: None,
                    link_impact: None,
                    finding_code: None,
                    repair_rule: None,
                },
                ApplyReportOp {
                    op_id: "1".into(),
                    kind: "rewrite_link".into(),
                    status: OpStatus::Failed,
                    from: None,
                    path: Some("b.md".into()),
                    stem: Some("b".into()),
                    summary: "rewrite [[old]] -> [[new]] in b.md: permission denied".into(),
                    error: None,
                    footnote: None,
                    cascade: None,
                    link_impact: None,
                    finding_code: None,
                    repair_rule: None,
                },
            ],
            warnings: Vec::new(),
            outcome: ApplyOutcome::Failed,
            touched_paths: Vec::new(),
        }
    }

    #[test]
    fn f1_a_failed_rewrite_wikilink_headlines_failed_not_rewrote() {
        let report = failed_report();
        let mut out = Vec::new();
        render_rewrite_records(&mut out, &report, "old", "new").unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "rewrite-wikilink failed\n  applied: 1  skipped: 0  failed: 1  remaining: 0\n  [applied] rewrote [[old]] -> [[new]] in a.md\n  [failed] rewrite [[old]] -> [[new]] in b.md: permission denied\n"
        );
    }
}
