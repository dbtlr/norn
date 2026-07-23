//! `move` (one of the cascade verbs) (NRN-409).
//!
//! Renders the shared `ApplyReport`. `--format json` is the report's PRETTY
//! serialization (with a trailing newline); records is the human
//! summary. A refused report renders the coded `{code,message,path?}`
//! envelope (json, pretty) or `error: <msg>` (records) and exits 2. Cascade
//! failures (real FS errors) surface on stderr before the summary.

use std::io::{self, Write};

use norn_wire::ApplyOutcome;

use crate::display::conversation::Conversation;
use crate::display::emit::render_outcome;
use crate::display::output::MoveMutationView;
use crate::display::sink::Sink;
use crate::display::{serde_label, Format};
use crate::output::glyphs::{self, Glyph};

use super::shared::{
    apply_report_exit, apply_status_label, plural, render_apply_refusal, render_cascade_failed,
    write_report_json,
};

pub(crate) fn render_move(
    view: MoveMutationView,
    format: Format,
    sink: &mut Sink<'_>,
    conv: &mut Conversation<'_>,
) -> i32 {
    let report = &view.report;
    let json = matches!(format, Format::Json);

    if report.outcome == ApplyOutcome::Refused {
        return render_apply_refusal(report, format, sink.writer(), conv);
    }

    conv.cascade_failure_warnings(report);
    let exit = apply_report_exit(report);

    if json {
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
        } else if report.outcome == ApplyOutcome::Failed {
            // A failed single-file move renders the truthful shared failure
            // block — the moved/preview wording below cannot express a report
            // whose op did not land.
            render_cascade_failed(sink.writer(), "move", report)?;
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
        // Skip an empty `trace:` line (empty-until-real posture); a confirmed
        // apply carries a real telemetry trace id.
        if !dry_run && !report.trace_id.is_empty() {
            sink.trace_footer(&report.trace_id)?;
            if report.telemetry_degraded {
                conv.telemetry_degraded_warning()?;
            }
        }
        Ok(exit)
    })();
    render_outcome(result, conv.writer())
}

/// Single-file move summary.
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

/// Folder move summary. The headline
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
        let status = serde_label(&op.status);
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
            telemetry_degraded: false,
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

    #[test]
    fn f1_a_failed_single_file_move_headlines_failed_not_preview() {
        use crate::display::format::{FormatChoice, FormatSpec};
        use crate::display::output::MoveMutationView;
        use crate::display::Presenter;
        use crate::output::palette::Palette;

        let mut report = failed_report();
        // A single-file move: one op, no folder provenance (`from` empty).
        report.applied = 0;
        report.trace_id = "abc123".into();
        report.operations = vec![ApplyReportOp {
            op_id: "0".into(),
            kind: "move_document".into(),
            status: OpStatus::Failed,
            from: None,
            path: Some("b/x.md".into()),
            stem: Some("x".into()),
            summary: "move a/x.md -> b/x.md: permission denied".into(),
            error: None,
            footnote: None,
            cascade: None,
            link_impact: None,
            finding_code: None,
            repair_rule: None,
        }];

        let view = MoveMutationView {
            report,
            src: "a/x.md".into(),
            dst: "b/x.md".into(),
            format: FormatChoice {
                explicit: Some(Format::Records),
                spec: FormatSpec {
                    tty: Format::Records,
                    piped: Format::Records,
                },
            },
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut presenter = Presenter::new(&mut out, &mut err);
            let format = view.format.resolve(false);
            let palette = Palette::off();
            let (o, e) = presenter.streams();
            let mut sink = Sink::new(o, &palette, 80);
            let mut conv = Conversation::new(e);
            render_move(view, format, &mut sink, &mut conv);
        }
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "move failed\n  applied: 0  skipped: 0  failed: 1  remaining: 0\n  [failed] move a/x.md -> b/x.md: permission denied\ntrace: abc123\n"
        );
    }
}
