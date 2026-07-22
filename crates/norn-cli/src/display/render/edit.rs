//! `edit` (body-edit report) (NRN-409).

use std::io;

use norn_wire::MutationOutcome;

use crate::display::conversation::Conversation;
use crate::display::emit::render_outcome;
use crate::display::format::Format;
use crate::display::output::EditMutationView;
use crate::display::sink::Sink;
use crate::display::{EXIT_OK, EXIT_USAGE};

pub(crate) fn render_edit(
    view: EditMutationView,
    format: Format,
    sink: &mut Sink<'_>,
    conv: &mut Conversation<'_>,
) -> i32 {
    let report = &view.report;

    // A refusal is format-INDEPENDENT for edit (the donor's pre-existing
    // asymmetry, `edit::route::emit`): `error: <message>` on stderr, exit 2, for
    // records AND json alike — unlike set/new, which emit a structured JSON
    // refusal object. `error.message` is the same `Display` prose the donor's
    // direct arm interpolated, so the two are byte-identical.
    if report.outcome == MutationOutcome::Refused {
        let msg = report
            .error
            .as_ref()
            .map(|e| e.message.clone())
            .unwrap_or_else(|| "edit refused".to_string());
        let result: io::Result<i32> = (|| {
            conv.line(&format!("error: {msg}"))?;
            Ok(EXIT_USAGE)
        })();
        return render_outcome(result, conv.writer());
    }

    // JSON: the compact whole-report serialization is the contract (donor
    // `render_json` — `serde_json::to_writer` + one trailing newline).
    if format == Format::Json {
        let result: io::Result<i32> = (|| {
            writeln!(sink.writer(), "{}", serde_json::to_string(report)?)?;
            Ok(EXIT_OK)
        })();
        return render_outcome(result, conv.writer());
    }

    // Records. Unstyled, like the donor `edit::report::render_records` (it never
    // resolved a palette), so the piped / parity bytes are exact.
    let result: io::Result<i32> = (|| {
        let verb = if report.applied {
            "edit"
        } else {
            "dry-run: edit"
        };
        writeln!(sink.writer(), "{verb} {}", report.target)?;
        for change in &report.edits {
            match change.occurrences {
                Some(n) => writeln!(sink.writer(), "  {} ({}, {n}×)", change.op, change.anchor)?,
                None => writeln!(sink.writer(), "  {} ({})", change.op, change.anchor)?,
            }
        }
        if report.body_changed {
            let old = report.body_bytes_old.unwrap_or(0);
            let new = report.body_bytes_new.unwrap_or(0);
            writeln!(sink.writer(), "  body: {old} → {new} bytes")?;
        }
        // The applied path prints a `trace:` footer after the records block
        // (records only; JSON carries `trace_id` as a field). A forecast prints
        // the blank line + `Apply with --yes` hint instead.
        if report.applied {
            writeln!(sink.writer(), "trace: {}", report.trace_id)?;
        } else {
            writeln!(sink.writer())?;
            writeln!(sink.writer(), "Apply with --yes")?;
        }
        Ok(EXIT_OK)
    })();
    render_outcome(result, conv.writer())
}
