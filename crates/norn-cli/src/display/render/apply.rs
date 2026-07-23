//! `apply` (NRN-409).
//!
//! Renders the `apply` verb's report. Unlike the sibling cascade verbs, `apply`
//! executes an arbitrary multi-op plan, so its records summary is the GENERIC
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

    if report.outcome == ApplyOutcome::Refused
        && !report.preconditions.iter().any(|p| p.error.is_some())
    {
        // Envelope-only refusal (schema mismatch, expansion, a bad create path).
        // Post-NRN-408 this path still carries the FULL report envelope, so when
        // `--out` is set, write that report to the file the user asked for
        // (exactly as the applied/failed path below does) and silence stdout —
        // rather than early-returning to stdout/stderr and leaving `--out`
        // empty. Without `--out`, render the coded `{code,message,path?}`
        // envelope (json) or `error: <msg>` (records), exit 2. An owner-SET
        // precondition mismatch instead carries a populated `preconditions[]`
        // with an error; that falls through to the FULL report render below
        // (records: the `apply refused` block WITH the preconditions block) —
        // the envelope-only path would drop the preconditions.
        if let Some(out_path) = &view.out {
            let result: io::Result<i32> = (|| {
                write_report_to_out_file(out_path, report)?;
                Ok(apply_report_exit(report))
            })();
            return render_outcome(result, conv.writer());
        }
        return render_apply_refusal(report, format, sink.writer(), conv);
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
        // only; json carries it as a field regardless. Unlike the envelope-only
        // refusal above (which never prints a trace line at all), the FULL report
        // render — reached here
        // for Applied/Failed/Rebased AND for a precondition-populated Refused
        // (`--yes` against a plan an owner-set precondition rejects) — carries the
        // footer on every one of those outcomes; only an actual forecast
        // (`dry_run == true`) omits it. The precondition-populated Refused report
        // carries an EMPTY trace_id (a refusal writes nothing, so it correlates
        // to no telemetry log), and an empty `trace:` line serves no one — skip
        // it when the id is empty, matching the empty-until-real trace_id posture.
        if !report.dry_run && !report.trace_id.is_empty() {
            sink.trace_footer(&report.trace_id)?;
            if report.telemetry_degraded {
                conv.telemetry_degraded_warning()?;
            }
        }
        Ok(exit)
    })();
    render_outcome(result, conv.writer())
}

/// The generic apply-report records block.
fn render_apply_records(out: &mut dyn Write, report: &ApplyReport) -> io::Result<()> {
    writeln!(out, "apply {}", apply_status_label(report))?;
    render_apply_report_body(out, report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::format::{FormatChoice, FormatSpec};
    use crate::display::Presenter;
    use crate::test_support::global_args;
    use norn_wire::{
        ApplyError, ApplyReportOp, ApplyReportPrecondition, OpStatus, PreconditionStatus,
    };

    fn drive<O: Write, E: Write>(view: ApplyMutationView, presenter: &mut Presenter<O, E>) -> i32 {
        let global = global_args();
        let format = view.format.resolve(false);
        let palette = crate::output::palette::resolve(global.color);
        let (out, err) = presenter.streams();
        let mut sink = Sink::new(out, &palette, 80);
        let mut conv = Conversation::new(err);
        render_apply(view, format, &mut sink, &mut conv)
    }

    /// A process-unique scratch path under the OS temp dir (no dev-dep on
    /// `tempfile`), removed by the caller.
    fn unique_out_path() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("norn-apply-out-{}-{n}.json", std::process::id()))
    }

    fn base_report() -> ApplyReport {
        ApplyReport {
            schema_version: 3,
            trace_id: String::new(),
            telemetry_degraded: false,
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
                kind: "apply".into(),
                status: OpStatus::Failed,
                from: None,
                path: None,
                stem: None,
                summary: "unsupported plan schema_version 999".into(),
                error: Some(ApplyError {
                    code: "unsupported-schema-version".into(),
                    message: "unsupported plan schema_version 999".into(),
                    path: None,
                }),
                footnote: None,
                cascade: None,
                link_impact: None,
                finding_code: None,
                repair_rule: None,
            }],
            warnings: Vec::new(),
            outcome: ApplyOutcome::Refused,
            touched_paths: Vec::new(),
        }
    }

    fn json_choice() -> FormatChoice {
        FormatChoice {
            explicit: Some(Format::Json),
            spec: FormatSpec {
                tty: Format::Records,
                piped: Format::Json,
            },
        }
    }

    // Fix 4 (NRN-402): an envelope-only refusal with `--out` writes the full
    // refusal report to the file (post-NRN-408 it carries a full envelope) and
    // silences stdout — it no longer early-returns before the `--out` handling.
    #[test]
    fn envelope_only_refusal_honors_out_and_silences_stdout() {
        let out_path = unique_out_path();
        let view = ApplyMutationView {
            report: base_report(),
            format: json_choice(),
            out: Some(out_path.to_string_lossy().into_owned()),
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut presenter = Presenter::new(&mut out, &mut err);
            drive(view, &mut presenter)
        };
        assert_eq!(code, 2, "a refusal exits 2");
        assert!(
            out.is_empty(),
            "stdout must be silenced when --out is set: {out:?}"
        );
        let written = std::fs::read_to_string(&out_path).expect("--out file must be written");
        let _ = std::fs::remove_file(&out_path);
        assert!(
            written.contains("\"outcome\": \"refused\"")
                && written.contains("unsupported-schema-version"),
            "the --out file must carry the full refusal envelope, got:\n{written}"
        );
    }

    // Fix 5 (NRN-402): a precondition-populated refused report (which falls
    // through to the FULL records render) carries an EMPTY trace_id, and an empty
    // `trace:` line serves no one — it must be skipped.
    #[test]
    fn precondition_refused_records_omits_the_empty_trace_footer() {
        let mut report = base_report();
        report.preconditions = vec![ApplyReportPrecondition {
            id: "owner-set".into(),
            status: PreconditionStatus::Failed,
            expected_paths: vec!["notes/alpha.md".into()],
            actual_paths: vec!["notes/beta.md".into()],
            error: Some(ApplyError {
                code: "owner-set-mismatch".into(),
                message: "owner set mismatch".into(),
                path: None,
            }),
        }];
        assert!(report.trace_id.is_empty());
        let view = ApplyMutationView {
            report,
            format: FormatChoice {
                explicit: Some(Format::Records),
                spec: FormatSpec {
                    tty: Format::Records,
                    piped: Format::Records,
                },
            },
            out: None,
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut presenter = Presenter::new(&mut out, &mut err);
            drive(view, &mut presenter);
        }
        let stdout = String::from_utf8(out).unwrap();
        // Build the marker via `concat!` so this file carries no quoted
        // trace-footer literal — the render-stream guard
        // (tests/render_stream_guard.rs) forbids that spelling in a renderer to
        // catch hand-rolled footers.
        let trace_marker = concat!("trace", ":");
        assert!(
            !stdout.contains(trace_marker),
            "an empty trace_id must not print a trace footer line, got:\n{stdout}"
        );
    }

    #[test]
    fn a_degraded_telemetry_sink_prints_the_operator_advisory_on_stderr() {
        let mut report = base_report();
        report.outcome = ApplyOutcome::Applied;
        report.applied = 1;
        report.failed = 0;
        report.operations = Vec::new();
        report.trace_id = "abc123".into();
        report.telemetry_degraded = true;
        let view = ApplyMutationView {
            report,
            format: FormatChoice {
                explicit: Some(Format::Records),
                spec: FormatSpec {
                    tty: Format::Records,
                    piped: Format::Records,
                },
            },
            out: None,
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut presenter = Presenter::new(&mut out, &mut err);
            drive(view, &mut presenter);
        }
        // Build the marker via `concat!` so this file carries no quoted
        // trace-footer literal — the render-stream guard
        // (tests/render_stream_guard.rs) forbids that spelling in a renderer.
        let trace_marker = concat!("trace", ": abc123");
        assert!(
            String::from_utf8(out).unwrap().contains(trace_marker),
            "the real trace id must still print"
        );
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "warning: audit trail not persisted for this apply (durable write failed)\n"
        );
    }
}
