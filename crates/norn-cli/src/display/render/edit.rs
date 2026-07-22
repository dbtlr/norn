//! `edit` (body-edit report) (NRN-409).

use std::io::{self, Write};

use norn_wire::MutationOutcome;

use crate::cli::GlobalArgs;
use crate::display::conversation::Conversation;
use crate::display::emit::{is_stdout_tty, render_outcome};
use crate::display::format::Format;
use crate::display::output::EditMutationView;
use crate::display::{Presenter, EXIT_OK, EXIT_USAGE};

pub(crate) fn render_edit<O: Write, E: Write>(
    view: EditMutationView,
    global: &GlobalArgs,
    presenter: &mut Presenter<O, E>,
) -> i32 {
    let format = view.spec.resolve(view.explicit, is_stdout_tty());
    let report = &view.report;
    let (out, err) = presenter.streams();
    let mut conv = Conversation::new(err);

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
            writeln!(out, "{}", serde_json::to_string(report)?)?;
            Ok(EXIT_OK)
        })();
        return render_outcome(result, conv.writer());
    }

    // Records. Unstyled, like the donor `edit::report::render_records` (it never
    // resolved a palette), so the piped / parity bytes are exact.
    let _ = global;
    let result: io::Result<i32> = (|| {
        let verb = if report.applied {
            "edit"
        } else {
            "dry-run: edit"
        };
        writeln!(out, "{verb} {}", report.target)?;
        for change in &report.edits {
            match change.occurrences {
                Some(n) => writeln!(out, "  {} ({}, {n}×)", change.op, change.anchor)?,
                None => writeln!(out, "  {} ({})", change.op, change.anchor)?,
            }
        }
        if report.body_changed {
            let old = report.body_bytes_old.unwrap_or(0);
            let new = report.body_bytes_new.unwrap_or(0);
            writeln!(out, "  body: {old} → {new} bytes")?;
        }
        // The applied path prints a `trace:` footer after the records block
        // (records only; JSON carries `trace_id` as a field). A forecast prints
        // the blank line + `Apply with --yes` hint instead.
        if report.applied {
            writeln!(out, "trace: {}", report.trace_id)?;
        } else {
            writeln!(out)?;
            writeln!(out, "Apply with --yes")?;
        }
        Ok(EXIT_OK)
    })();
    render_outcome(result, conv.writer())
}
