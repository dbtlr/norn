//! `move` (one of the cascade verbs) (NRN-409).
//!
//! Renders the shared `ApplyReport` the donor emits (byte-faithful to
//! `retired/src/move/route.rs`). `--format json` is the report's PRETTY
//! serialization (with a trailing newline); records is the donor's human
//! summary. A refused report renders the coded `{code,message,path?}`
//! envelope (json, pretty) or `error: <msg>` (records) and exits 2. Cascade
//! failures (real FS errors) surface on stderr before the summary.

use std::io::{self, Write};

use norn_wire::ApplyOutcome;

use crate::display::conversation::Conversation;
use crate::display::emit::render_outcome;
use crate::display::output::MoveMutationView;
use crate::display::sink::Sink;
use crate::output::glyphs::{self, Glyph};

use super::shared::{
    apply_report_exit, apply_status_label, plural, render_apply_refusal, write_report_json,
};

pub(crate) fn render_move(
    view: MoveMutationView,
    sink: &mut Sink<'_>,
    conv: &mut Conversation<'_>,
) -> i32 {
    let report = &view.report;

    if report.outcome == ApplyOutcome::Refused {
        return render_apply_refusal(report, view.json, sink.writer(), conv);
    }

    conv.cascade_failure_warnings(report);
    let exit = apply_report_exit(report);

    if view.json {
        let result: io::Result<i32> = (|| {
            write_report_json(sink.writer(), report)?;
            Ok(exit)
        })();
        return render_outcome(result, conv.writer());
    }

    let ascii = glyphs::use_ascii();
    let dry_run = report.dry_run;
    // A folder move's expanded ops carry `from` provenance (the parent
    // `move_folder` op index); a single-file move's `move_document` op does not.
    let is_folder = report.operations.iter().any(|o| o.from.is_some());
    let result: io::Result<i32> = (|| {
        if is_folder {
            render_folder_apply_tty(sink.writer(), report)?;
        } else {
            let (link_total, link_files) = report
                .operations
                .iter()
                .find(|o| o.kind == "move_document")
                .and_then(|o| o.cascade.as_ref())
                .map_or((0, 0), |c| (c.applied, c.files));
            let applied = !dry_run && exit == 0;
            render_move_apply_tty(
                sink.writer(),
                &view.src,
                &view.dst,
                link_total,
                link_files,
                applied,
                ascii,
            )?;
        }
        if !dry_run {
            sink.trace_footer(&report.trace_id)?;
        }
        Ok(exit)
    })();
    render_outcome(result, conv.writer())
}

/// Single-file move summary (donor `r#move::render_move_apply_tty`).
fn render_move_apply_tty(
    out: &mut dyn Write,
    src: &str,
    dst: &str,
    link_total: usize,
    link_files: usize,
    applied: bool,
    ascii: bool,
) -> io::Result<()> {
    let arrow = glyphs::render(Glyph::Arrow, ascii);
    if applied {
        writeln!(
            out,
            "{} moved {src} {arrow} {dst}",
            glyphs::render(Glyph::Pass, ascii)
        )?;
        if link_total > 0 {
            writeln!(
                out,
                "{} rewrote {} backlink{} across {} file{}",
                glyphs::render(Glyph::Pass, ascii),
                link_total,
                plural(link_total),
                link_files,
                plural(link_files),
            )?;
        }
    } else {
        writeln!(out, "norn move {src} {arrow} {dst}")?;
        if link_total > 0 {
            writeln!(
                out,
                "  {} backlink{} to rewrite across {} file{}",
                link_total,
                plural(link_total),
                link_files,
                plural(link_files),
            )?;
        } else {
            writeln!(out, "  no backlinks to rewrite")?;
        }
    }
    Ok(())
}

/// Folder move summary (donor `r#move::render_folder_apply_tty`). The headline
/// reads the report's own outcome (`apply_status_label`), not `dry_run` alone,
/// so a runtime op failure prints `move-folder failed` rather than the
/// `applied` label a purely dry-run-keyed check would still emit.
fn render_folder_apply_tty(out: &mut dyn Write, report: &norn_wire::ApplyReport) -> io::Result<()> {
    writeln!(out, "move-folder {}", apply_status_label(report))?;
    writeln!(
        out,
        "  applied: {}  skipped: {}  failed: {}",
        report.applied, report.skipped, report.failed
    )?;
    for op in &report.operations {
        let status = format!("{:?}", op.status).to_lowercase();
        writeln!(out, "  [{status}] {}", op.summary)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use norn_wire::{ApplyReport, ApplyReportOp, OpStatus};

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
                    kind: "move_document".into(),
                    status: OpStatus::Applied,
                    from: Some("0".into()),
                    path: Some("b/x.md".into()),
                    stem: Some("x".into()),
                    summary: "moved a/x.md -> b/x.md".into(),
                    error: None,
                    footnote: None,
                    cascade: None,
                    link_impact: None,
                    finding_code: None,
                    repair_rule: None,
                },
                ApplyReportOp {
                    op_id: "1".into(),
                    kind: "move_document".into(),
                    status: OpStatus::Failed,
                    from: Some("0".into()),
                    path: Some("b/y.md".into()),
                    stem: Some("y".into()),
                    summary: "move a/y.md -> b/y.md: permission denied".into(),
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
    fn f1_a_failed_folder_move_headlines_failed_not_applied() {
        let report = failed_report();
        let mut out = Vec::new();
        render_folder_apply_tty(&mut out, &report).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "move-folder failed\n  applied: 1  skipped: 0  failed: 1\n  [applied] moved a/x.md -> b/x.md\n  [failed] move a/y.md -> b/y.md: permission denied\n"
        );
    }
}
