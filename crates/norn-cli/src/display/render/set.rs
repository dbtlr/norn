//! `set` (mutation report) (NRN-409).

use std::io::{self, Write};

use norn_wire::{FrontmatterChange, MutationOutcome};

use crate::display::conversation::Conversation;
use crate::display::emit::render_outcome;
use crate::display::format::Format;
use crate::display::output::SetMutationView;
use crate::display::sink::Sink;
use crate::output::glyphs;
use crate::output::palette::Palette;
use crate::output::primitives;

use super::shared::{mutation_exit, value_repr, warning_short};

pub(crate) fn render_set(
    view: SetMutationView,
    format: Format,
    sink: &mut Sink<'_>,
    conv: &mut Conversation<'_>,
) -> i32 {
    let report = &view.report;

    // JSON: the compact whole-report serialization is the contract (struct field
    // order, donor-faithful). Structured on refusal too (ADR 0016 unifies the
    // surfaces on the structured envelope).
    if format == Format::Json {
        let result: io::Result<i32> = (|| {
            writeln!(sink.writer(), "{}", serde_json::to_string(report)?)?;
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
            render_frontmatter_change(sink.writer(), &palette, change, ascii)?;
        }
        if report.body_changed {
            let old = report.body_bytes_old.unwrap_or(0);
            let new = report.body_bytes_new.unwrap_or(0);
            primitives::change_line(
                sink.writer(),
                &palette,
                "body",
                &format!("{old} bytes"),
                Some(&format!("{new} bytes")),
                ascii,
            )?;
        }
        // Warnings block (donor: on STDOUT): `  warnings: N` then `    - <short>`
        // for the first three, then `    … (K more)`.
        if !report.warnings.is_empty() {
            let out = sink.writer();
            writeln!(out, "  warnings: {}", report.warnings.len())?;
            for w in report.warnings.iter().take(3) {
                writeln!(out, "    - {}", warning_short(w))?;
            }
            if report.warnings.len() > 3 {
                writeln!(out, "    … ({} more)", report.warnings.len() - 3)?;
            }
        }
        if report.applied {
            writeln!(sink.writer(), "trace: {}", report.trace_id)?;
        } else {
            writeln!(sink.writer())?;
            writeln!(sink.writer(), "Apply with --yes")?;
        }
        Ok(crate::display::EXIT_OK)
    })();
    render_outcome(result, conv.writer())
}

/// One `set` change line, dispatched by the normalized op. An absent prior value
/// (a field added by an upsert) renders its `before` as `<none>` (donor).
fn render_frontmatter_change(
    out: &mut dyn Write,
    palette: &Palette,
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
            primitives::change_line(out, palette, field, &before, Some(&after), ascii)
        }
        "remove" => {
            let was = change.old.as_ref().map(value_repr).unwrap_or_default();
            primitives::change_line(
                out,
                palette,
                field,
                &format!("remove (was {was})"),
                None,
                ascii,
            )
        }
        "push" => {
            let v = change.value.as_ref().map(value_repr).unwrap_or_default();
            primitives::change_line(out, palette, field, &format!("push {v}"), None, ascii)
        }
        "pop" => {
            let v = change.value.as_ref().map(value_repr).unwrap_or_default();
            let found = change.found.unwrap_or(false);
            primitives::change_line(
                out,
                palette,
                field,
                &format!("pop {v} (found: {found})"),
                None,
                ascii,
            )
        }
        other => primitives::change_line(out, palette, field, other, None, ascii),
    }
}
