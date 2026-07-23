//! `edit` (body-edit report) (NRN-409).

use std::io;

use norn_wire::MutationOutcome;

use crate::display::conversation::Conversation;
use crate::display::emit::render_outcome;
use crate::display::format::Format;
use crate::display::output::EditMutationView;
use crate::display::sink::Sink;
use crate::display::EXIT_USAGE;

use super::shared::{mutation_exit, write_report_json};

pub(crate) fn render_edit(
    view: EditMutationView,
    format: Format,
    sink: &mut Sink<'_>,
    conv: &mut Conversation<'_>,
) -> i32 {
    let report = &view.report;

    // JSON: the whole-report serialization under the one mutation-report policy
    // (`write_report_json`: pretty, struct-field order, one trailing newline),
    // on EVERY outcome path INCLUDING refusal. `edit` previously refused as bare
    // `error: <message>` prose even under `--format json`; it now carries the
    // same full envelope (`outcome: refused` + the coded error) `set` / `new` /
    // the cascade verbs do, so a JSON consumer parses one shape on every path.
    if format == Format::Json {
        let result: io::Result<i32> = (|| {
            write_report_json(sink.writer(), report)?;
            Ok(mutation_exit(report.outcome))
        })();
        return render_outcome(result, conv.writer());
    }

    // Records: a refusal prints `error: <message>` on stderr, exit 2. The
    // message is the same `Display` prose the routed and direct arms interpolate.
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

    // Records. Unstyled (never resolves a palette), so the piped bytes are
    // deterministic.
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
            sink.trace_footer(&report.trace_id)?;
        } else {
            writeln!(sink.writer())?;
            writeln!(sink.writer(), "Apply with --yes")?;
        }
        Ok(crate::display::EXIT_OK)
    })();
    render_outcome(result, conv.writer())
}
