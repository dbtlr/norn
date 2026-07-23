//! `delete` (one of the cascade verbs) (NRN-409).
//!
//! Renders the shared `ApplyReport` the donor emits (byte-faithful to
//! `retired/src/delete/route.rs`). `--format json` is the report's PRETTY
//! serialization (with a trailing newline); records is the donor's human
//! summary. A refused report renders the coded `{code,message,path?}`
//! envelope (json, pretty) or `error: <msg>` (records) and exits 2. Cascade
//! failures (real FS errors) surface on stderr before the summary.

use std::io::{self, Write};

use norn_wire::{ApplyOutcome, ApplyReport};

use crate::display::conversation::Conversation;
use crate::display::emit::render_outcome;
use crate::display::output::DeleteMutationView;
use crate::display::sink::Sink;
use crate::output::glyphs::{self, Glyph};

use super::shared::{
    apply_report_exit, noun, render_apply_refusal, render_cascade_failed, write_report_json,
};

pub(crate) fn render_delete(
    view: DeleteMutationView,
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
    let result: io::Result<i32> = (|| {
        render_delete_records(sink.writer(), report, &view.doc, dry_run, exit, ascii)?;
        if !dry_run {
            sink.trace_footer(&report.trace_id)?;
        }
        Ok(exit)
    })();
    render_outcome(result, conv.writer())
}

/// Records-format delete output (donor `delete::render_delete_records`). A
/// runtime op failure (`outcome = failed`) renders the truthful shared
/// `render_cascade_failed` headline instead of the applied/preview wording
/// below — nothing here can tell "deleted" from "failed to delete" once a real
/// FS error happened, so the shared block owns that state.
fn render_delete_records(
    out: &mut dyn Write,
    report: &ApplyReport,
    doc: &str,
    dry_run: bool,
    exit: i32,
    ascii: bool,
) -> io::Result<()> {
    if report.outcome == ApplyOutcome::Failed {
        return render_cascade_failed(out, "delete", report);
    }
    let delete_op = report
        .operations
        .iter()
        .find(|o| o.kind == "delete_document");
    let (incoming_total, incoming_files, redirect_to): (usize, &[String], Option<&str>) = delete_op
        .and_then(|o| o.link_impact.as_ref())
        .map_or((0, &[][..], None), |li| {
            (
                li.incoming_total,
                li.incoming_files.as_slice(),
                li.redirect_to.as_deref(),
            )
        });
    let rewrite_total = delete_op
        .and_then(|o| o.cascade.as_ref())
        .map_or(0, |c| c.applied);
    let applied = !dry_run && exit == 0;
    render_delete_apply_tty(
        out,
        doc,
        incoming_total,
        incoming_files,
        redirect_to,
        rewrite_total,
        applied,
        ascii,
    )?;
    Ok(())
}

/// The delete summary (donor `delete::render_delete_apply_tty`).
#[allow(clippy::too_many_arguments)]
fn render_delete_apply_tty(
    out: &mut dyn Write,
    doc: &str,
    incoming_total: usize,
    incoming_files: &[String],
    rewrite_to: Option<&str>,
    rewrite_total: usize,
    applied: bool,
    ascii: bool,
) -> io::Result<()> {
    let pass = glyphs::render(Glyph::Pass, ascii);
    let warn = glyphs::render(Glyph::Warn, ascii);
    if applied {
        match rewrite_to {
            Some(alt) => {
                writeln!(
                    out,
                    "{pass} deleted {doc} (incoming links redirected to {alt})"
                )?;
                writeln!(
                    out,
                    "{pass} rewrote {} {} across {} {}",
                    rewrite_total,
                    noun(rewrite_total, "backlink", "backlinks"),
                    incoming_files.len(),
                    noun(incoming_files.len(), "file", "files"),
                )?;
            }
            None => {
                writeln!(out, "{pass} deleted {doc}")?;
                if incoming_total > 0 {
                    writeln!(
                        out,
                        "{warn} {} {} now broken (surface via norn validate)",
                        incoming_total,
                        noun(incoming_total, "link", "links"),
                    )?;
                }
            }
        }
    } else {
        match rewrite_to {
            Some(alt) => {
                writeln!(
                    out,
                    "norn delete {doc} {} redirects {} incoming {} to {alt}",
                    glyphs::render(Glyph::Arrow, ascii),
                    incoming_total,
                    noun(incoming_total, "link", "links"),
                )?;
                writeln!(
                    out,
                    "  {} {} to rewrite across {} {}",
                    rewrite_total,
                    noun(rewrite_total, "backlink", "backlinks"),
                    incoming_files.len(),
                    noun(incoming_files.len(), "file", "files"),
                )?;
            }
            None => {
                writeln!(out, "norn delete {doc}")?;
                if incoming_total > 0 {
                    writeln!(
                        out,
                        "  {warn} {} incoming {} will break across {} {}:",
                        incoming_total,
                        noun(incoming_total, "link", "links"),
                        incoming_files.len(),
                        noun(incoming_files.len(), "file", "files"),
                    )?;
                    for file in incoming_files {
                        writeln!(out, "      {file}")?;
                    }
                    writeln!(
                        out,
                        "  (broken links will surface as link-target-missing findings in `norn validate`)"
                    )?;
                }
            }
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
            applied: 0,
            skipped: 0,
            failed: 1,
            remaining: 0,
            preconditions: Vec::new(),
            operations: vec![ApplyReportOp {
                op_id: "0".into(),
                kind: "delete_document".into(),
                status: OpStatus::Failed,
                from: None,
                path: Some("doc.md".into()),
                stem: Some("doc".into()),
                summary: "delete doc.md: permission denied".into(),
                error: None,
                footnote: None,
                cascade: None,
                link_impact: None,
                finding_code: None,
                repair_rule: None,
            }],
            warnings: Vec::new(),
            outcome: ApplyOutcome::Failed,
            touched_paths: Vec::new(),
        }
    }

    #[test]
    fn f1_a_failed_delete_renders_the_truthful_failed_headline_not_a_preview() {
        let report = failed_report();
        let mut out = Vec::new();
        render_delete_records(&mut out, &report, "doc.md", false, 1, false).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "delete failed\n  applied: 0  skipped: 0  failed: 1  remaining: 0\n  [failed] delete doc.md: permission denied\n"
        );
    }
}
