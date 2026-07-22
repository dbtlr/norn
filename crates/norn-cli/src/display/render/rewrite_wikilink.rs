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

use super::shared::{apply_report_exit, emit_cascade_failure_warnings, render_apply_refusal};

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

    emit_cascade_failure_warnings(report, conv.writer());
    let exit = apply_report_exit(report);

    // `--out`: write the (always-JSON, pretty) report to the file, silence stdout.
    if let Some(out_path) = &view.out {
        let result: io::Result<i32> = (|| {
            let json = serde_json::to_string_pretty(report)?;
            std::fs::write(out_path, format!("{json}\n"))?;
            Ok(exit)
        })();
        return render_outcome(result, conv.writer());
    }

    if view.json {
        let result: io::Result<i32> = (|| {
            writeln!(sink.writer(), "{}", serde_json::to_string_pretty(report)?)?;
            Ok(exit)
        })();
        return render_outcome(result, conv.writer());
    }

    let result: io::Result<i32> = (|| {
        render_rewrite_records(sink.writer(), report, &view.old, &view.new)?;
        if !report.dry_run {
            writeln!(sink.writer(), "trace: {}", report.trace_id)?;
        }
        Ok(exit)
    })();
    render_outcome(result, conv.writer())
}

/// Records-format rewrite-wikilink output (donor `rewrite_wikilink::render_records`).
fn render_rewrite_records(
    out: &mut dyn Write,
    report: &ApplyReport,
    old: &str,
    new: &str,
) -> io::Result<()> {
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
