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
use crate::display::Presenter;
use crate::output::glyphs::{self, Glyph};

use super::shared::{apply_report_exit, emit_cascade_failure_warnings, noun, render_apply_refusal};

pub(crate) fn render_delete<O: Write, E: Write>(
    view: DeleteMutationView,
    presenter: &mut Presenter<O, E>,
) -> i32 {
    let report = &view.report;
    let (out, err) = presenter.streams();
    let mut conv = Conversation::new(err);

    if report.outcome == ApplyOutcome::Refused {
        return render_apply_refusal(report, view.json, out, &mut conv);
    }

    emit_cascade_failure_warnings(report, conv.writer());
    let exit = apply_report_exit(report);

    if view.json {
        let result: io::Result<i32> = (|| {
            writeln!(out, "{}", serde_json::to_string_pretty(report)?)?;
            Ok(exit)
        })();
        return render_outcome(result, conv.writer());
    }

    let ascii = glyphs::use_ascii();
    let dry_run = report.dry_run;
    let result: io::Result<i32> = (|| {
        render_delete_records(out, report, &view.doc, dry_run, exit, ascii)?;
        Ok(exit)
    })();
    render_outcome(result, conv.writer())
}

/// Records-format delete output (donor `delete::render_delete_records`).
fn render_delete_records(
    out: &mut dyn Write,
    report: &ApplyReport,
    doc: &str,
    dry_run: bool,
    exit: i32,
    ascii: bool,
) -> io::Result<()> {
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
    if !dry_run {
        writeln!(out, "trace: {}", report.trace_id)?;
    }
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
