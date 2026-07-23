//! `set` (mutation report) (NRN-409).

use std::io;

use norn_wire::{FrontmatterChange, MutationOutcome};

use crate::display::conversation::Conversation;
use crate::display::emit::render_outcome;
use crate::display::format::Format;
use crate::display::output::SetMutationView;
use crate::display::sink::Sink;
use crate::output::glyphs;

use super::shared::{mutation_exit, value_repr, warning_short, write_report_json};

pub(crate) fn render_set(
    view: SetMutationView,
    format: Format,
    sink: &mut Sink<'_>,
    conv: &mut Conversation<'_>,
) -> i32 {
    let report = &view.report;

    // JSON: the whole-report serialization under the one mutation-report policy
    // (`write_report_json`: pretty, struct-field order, one trailing newline),
    // on EVERY outcome path including refusal — the full envelope carries the
    // coded error, never a bare `{code,message}` fragment (ADR 0016 unifies the
    // surfaces on the structured envelope).
    if format == Format::Json {
        let result: io::Result<i32> = (|| {
            write_report_json(sink.writer(), report)?;
            Ok(mutation_exit(report.outcome))
        })();
        return render_outcome(result, conv.writer());
    }

    // Records. A refusal prints the coded error on stderr (exit 2).
    if report.outcome == MutationOutcome::Refused {
        let msg = report
            .error
            .as_ref()
            .map(|e| e.message.clone())
            .unwrap_or_else(|| "set refused".to_string());
        let result: io::Result<i32> = (|| {
            conv.line(&format!("error: {msg}"))?;
            Ok(crate::display::EXIT_USAGE)
        })();
        return render_outcome(result, conv.writer());
    }

    let palette = *sink.palette();
    let ascii = glyphs::use_ascii();
    let result: io::Result<i32> = (|| {
        let verb = if report.applied {
            "set"
        } else {
            "dry-run: set"
        };
        writeln!(
            sink.writer(),
            "{}{verb} {}{}",
            palette.header.render(),
            report.target,
            palette.header.render_reset()
        )?;
        for change in &report.frontmatter_changes {
            render_frontmatter_change(sink, change, ascii)?;
        }
        if report.body_changed {
            let old = report.body_bytes_old.unwrap_or(0);
            let new = report.body_bytes_new.unwrap_or(0);
            sink.change_line(
                "body",
                &format!("{old} bytes"),
                Some(&format!("{new} bytes")),
                ascii,
            )?;
        }
        // Warnings block (on STDOUT): `  warnings: N` then `    - <short>`
        // for the first three, then `    … (K more)`.
        if !report.warnings.is_empty() {
            let shorts: Vec<String> = report.warnings.iter().map(warning_short).collect();
            sink.mutation_warnings_block(&shorts)?;
        }
        // A confirmed `set` routes through the shared `apply_migration_plan`
        // executor (NRN-400), which mints a real `EventSink`-derived trace id —
        // never the empty placeholder a pre-NRN-400 forecast-only wiring would
        // have left here.
        if report.applied {
            sink.trace_footer(&report.trace_id)?;
            if report.telemetry_degraded {
                conv.telemetry_degraded_warning()?;
            }
        } else {
            writeln!(sink.writer())?;
            writeln!(sink.writer(), "Apply with --yes")?;
        }
        Ok(crate::display::EXIT_OK)
    })();
    render_outcome(result, conv.writer())
}

/// One `set` change line, dispatched by the normalized op. An absent prior value
/// (a field added by an upsert) renders its `before` as `<none>`.
fn render_frontmatter_change(
    sink: &mut Sink<'_>,
    change: &FrontmatterChange,
    ascii: bool,
) -> io::Result<()> {
    let field = change.field.as_str();
    match change.op.as_str() {
        "set" => {
            let before = change
                .old
                .as_ref()
                .map(value_repr)
                .unwrap_or_else(|| "<none>".to_string());
            let after = change.new.as_ref().map(value_repr).unwrap_or_default();
            sink.change_line(field, &before, Some(&after), ascii)
        }
        "remove" => {
            let was = change.old.as_ref().map(value_repr).unwrap_or_default();
            sink.change_line(field, &format!("remove (was {was})"), None, ascii)
        }
        "push" => {
            let v = change.value.as_ref().map(value_repr).unwrap_or_default();
            sink.change_line(field, &format!("push {v}"), None, ascii)
        }
        "pop" => {
            let v = change.value.as_ref().map(value_repr).unwrap_or_default();
            let found = change.found.unwrap_or(false);
            sink.change_line(field, &format!("pop {v} (found: {found})"), None, ascii)
        }
        other => sink.change_line(field, other, None, ascii),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    use crate::display::format::{FormatChoice, FormatSpec};
    use crate::display::Presenter;
    use crate::output::palette::Palette;
    use norn_wire::{CodedError, SetReport};
    use serde_json::json;

    /// Drive `render_set` through the same resolution `emit` performs — a
    /// no-op palette sink, matching `render_new`'s test harness.
    fn drive<O: Write, E: Write>(view: SetMutationView, presenter: &mut Presenter<O, E>) -> i32 {
        let format = view.format.resolve(false);
        let palette = Palette::off();
        let (out, err) = presenter.streams();
        let mut sink = Sink::new(out, &palette, 80);
        let mut conv = Conversation::new(err);
        render_set(view, format, &mut sink, &mut conv)
    }

    fn base_report(applied: bool) -> SetReport {
        SetReport {
            schema_version: 2,
            trace_id: "abc123".into(),
            telemetry_degraded: false,
            operation: "set".into(),
            target: "note.md".into(),
            frontmatter_changes: Vec::new(),
            body_changed: false,
            body_bytes_new: None,
            body_bytes_old: None,
            applied,
            outcome: MutationOutcome::Applied,
            error: None,
            warnings: Vec::new(),
        }
    }

    fn view(report: SetReport) -> SetMutationView {
        SetMutationView {
            report,
            format: FormatChoice {
                explicit: Some(Format::Records),
                spec: FormatSpec {
                    tty: Format::Records,
                    piped: Format::Records,
                },
            },
        }
    }

    fn render(report: SetReport) -> String {
        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut presenter = Presenter::new(&mut out, &mut err);
            drive(view(report), &mut presenter);
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn set_change_prints_the_before_arrow_after_change_line() {
        let mut report = base_report(true);
        report.frontmatter_changes.push(FrontmatterChange {
            op: "set".into(),
            field: "status".into(),
            old: Some(json!("draft")),
            new: Some(json!("done")),
            value: None,
            found: None,
        });
        assert_eq!(
            render(report),
            "set note.md\n  status: draft → done\ntrace: abc123\n"
        );
    }

    #[test]
    fn remove_change_prints_the_was_value_and_dry_run_tails_with_apply_hint() {
        let mut report = base_report(false);
        report.frontmatter_changes.push(FrontmatterChange {
            op: "remove".into(),
            field: "tags".into(),
            old: Some(json!(["a", "b"])),
            new: None,
            value: None,
            found: None,
        });
        assert_eq!(
            render(report),
            "dry-run: set note.md\n  tags: remove (was [\"a\",\"b\"])\n\nApply with --yes\n"
        );
    }

    #[test]
    fn push_change_prints_the_pushed_value() {
        let mut report = base_report(true);
        report.frontmatter_changes.push(FrontmatterChange {
            op: "push".into(),
            field: "aliases".into(),
            old: None,
            new: None,
            value: Some(json!("Alt Name")),
            found: None,
        });
        assert_eq!(
            render(report),
            "set note.md\n  aliases: push Alt Name\ntrace: abc123\n"
        );
    }

    #[test]
    fn pop_change_prints_the_popped_value_and_whether_it_was_found() {
        let mut report = base_report(true);
        report.frontmatter_changes.push(FrontmatterChange {
            op: "pop".into(),
            field: "queue".into(),
            old: None,
            new: None,
            value: Some(json!(3)),
            found: Some(true),
        });
        assert_eq!(
            render(report),
            "set note.md\n  queue: pop 3 (found: true)\ntrace: abc123\n"
        );
    }

    #[test]
    fn a_refused_set_prints_the_coded_error_on_stderr_and_exits_usage() {
        let mut report = base_report(false);
        report.outcome = MutationOutcome::Refused;
        report.error = Some(CodedError {
            code: "target-not-found".into(),
            message: "target not found: missing.md".into(),
            path: None,
        });
        let mut out = Vec::new();
        let mut err = Vec::new();
        let exit = {
            let mut presenter = Presenter::new(&mut out, &mut err);
            drive(view(report), &mut presenter)
        };
        assert_eq!(exit, crate::display::EXIT_USAGE);
        assert!(out.is_empty(), "{out:?}");
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "error: target not found: missing.md\n"
        );
    }

    #[test]
    fn a_degraded_telemetry_sink_prints_the_operator_advisory_on_stderr() {
        let mut report = base_report(true);
        report.telemetry_degraded = true;
        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut presenter = Presenter::new(&mut out, &mut err);
            drive(view(report), &mut presenter);
        }
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "set note.md\ntrace: abc123\n"
        );
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "warning: audit trail not persisted for this apply (durable write failed)\n"
        );
    }

    #[test]
    fn a_healthy_telemetry_sink_prints_no_advisory() {
        let report = base_report(true);
        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut presenter = Presenter::new(&mut out, &mut err);
            drive(view(report), &mut presenter);
        }
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "set note.md\ntrace: abc123\n"
        );
        assert!(
            String::from_utf8(err).unwrap().is_empty(),
            "no advisory when telemetry_degraded is false"
        );
    }
}
