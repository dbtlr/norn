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
    apply_report_exit, emit_cascade_failure_warnings, plural, render_apply_refusal,
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

    emit_cascade_failure_warnings(report, conv.writer());
    let exit = apply_report_exit(report);

    if view.json {
        let result: io::Result<i32> = (|| {
            writeln!(sink.writer(), "{}", serde_json::to_string_pretty(report)?)?;
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
            render_folder_apply_tty(sink.writer(), report, dry_run)?;
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
            writeln!(sink.writer(), "trace: {}", report.trace_id)?;
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

/// Folder move summary (donor `r#move::render_folder_apply_tty`).
fn render_folder_apply_tty(
    out: &mut dyn Write,
    report: &norn_wire::ApplyReport,
    dry_run: bool,
) -> io::Result<()> {
    let status_label = if dry_run { "dry-run" } else { "applied" };
    writeln!(out, "move-folder {status_label}")?;
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
