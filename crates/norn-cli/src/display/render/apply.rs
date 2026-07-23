//! `apply` (NRN-409).
//!
//! Renders the `apply` verb's report (donor `apply::render_report` +
//! `render_records`). Unlike the sibling cascade verbs, `apply` executes an
//! arbitrary multi-op plan, so its records summary is the donor's GENERIC
//! apply-report block (`apply <status>` + counts + preconditions + per-op +
//! warnings), not a verb-specific line. Shares the json path, `--out` write, the
//! refusal envelope, and the cascade-failure warnings with the other cascade
//! verbs — the one apply-report render surface.

use std::io::{self, Write};

use norn_wire::{ApplyOutcome, ApplyReport};

use crate::display::conversation::Conversation;
use crate::display::emit::render_outcome;
use crate::display::output::ApplyMutationView;
use crate::display::sink::Sink;
use crate::display::Format;

use super::shared::{
    apply_report_exit, apply_status_label, render_apply_refusal, render_apply_report_body,
    write_report_json, write_report_to_out_file,
};

pub(crate) fn render_apply(
    view: ApplyMutationView,
    format: Format,
    sink: &mut Sink<'_>,
    conv: &mut Conversation<'_>,
) -> i32 {
    let report = &view.report;
    let json = matches!(format, Format::Json);

    if report.outcome == ApplyOutcome::Refused {
        // Envelope-only refusal (schema mismatch, expansion, a bad create path):
        // the coded `{code,message,path?}` envelope (json) or `error: <msg>`
        // (records), exit 2 — donor `emit_refusal`. An owner-SET precondition
        // mismatch instead carries a populated `preconditions[]` with an error;
        // that falls through to the FULL report render below (records: the
        // `apply refused` block WITH the preconditions block; honoring `--out`),
        // exactly as the donor's routed `emit` does — `emit_refusal` would drop
        // the preconditions.
        if !report.preconditions.iter().any(|p| p.error.is_some()) {
            return render_apply_refusal(report, format, sink.writer(), conv);
        }
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

    if json {
        let result: io::Result<i32> = (|| {
            write_report_json(sink.writer(), report)?;
            Ok(exit)
        })();
        return render_outcome(result, conv.writer());
    }

    let result: io::Result<i32> = (|| {
        render_apply_records(sink.writer(), report)?;
        // TTY `trace:` footer on any CONFIRMED (non-dry-run) apply call — records
        // only; json carries it as a field regardless. Empirically confirmed
        // against the pinned oracle: unlike the envelope-only refusal above (which
        // never prints a trace line at all), the FULL report render — reached here
        // for Applied/Failed/Rebased AND for a precondition-populated Refused
        // (`--yes` against a plan an owner-set precondition rejects) — carries the
        // footer on every one of those outcomes, including Refused; only an actual
        // forecast (`dry_run == true`) omits it.
        if !report.dry_run {
            sink.trace_footer(&report.trace_id)?;
        }
        Ok(exit)
    })();
    render_outcome(result, conv.writer())
}

/// The generic apply-report records block (donor `apply::render_records`).
fn render_apply_records(out: &mut dyn Write, report: &ApplyReport) -> io::Result<()> {
    writeln!(out, "apply {}", apply_status_label(report))?;
    render_apply_report_body(out, report)
}
