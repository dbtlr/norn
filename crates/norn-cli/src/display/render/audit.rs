//! `audit` (NRN-400) — render the mutation event stream.
//!
//! `records` (default) is a vertical key-value block per event, newest-first;
//! `json` is the flattened projection array, or — with `--raw` — the stored
//! OTEL Logs objects verbatim. An empty stream renders nothing (records) or
//! `[]` (json), each exit 0.

use std::fmt::Write as _;
use std::io;

use norn_wire::{AuditEvent, AuditReport};

use crate::display::conversation::Conversation;
use crate::display::emit::render_outcome;
use crate::display::format::Format;
use crate::display::output::AuditView;
use crate::display::sink::Sink;
use crate::display::EXIT_OK;

pub(crate) fn render_audit(
    view: AuditView,
    format: Format,
    sink: &mut Sink<'_>,
    conv: &mut Conversation<'_>,
) -> i32 {
    let text = match format {
        Format::Json => audit_json(&view.report, view.raw),
        _ => audit_records(&view.report),
    };
    let result: io::Result<i32> = (|| {
        if text.is_empty() {
            // An empty records stream writes nothing at all — no trailing blank
            // line — and still exits 0.
        } else if text.ends_with('\n') {
            write!(sink.writer(), "{text}")?;
        } else {
            writeln!(sink.writer(), "{text}")?;
        }
        Ok(EXIT_OK)
    })();
    render_outcome(result, conv.writer())
}

/// The JSON array: flattened projection per event, or the raw OTEL objects when
/// `--raw` is set. Always a valid array (an empty stream is `[]`).
fn audit_json(report: &AuditReport, raw: bool) -> String {
    let array: Vec<serde_json::Value> = if raw {
        report.events.iter().map(|e| e.raw.clone()).collect()
    } else {
        report.events.iter().map(AuditEvent::flatten).collect()
    };
    serde_json::to_string(&array).unwrap_or_else(|_| "[]".to_string())
}

/// One vertical key-value block per event, blank-line separated, newest-first.
fn audit_records(report: &AuditReport) -> String {
    let mut buf = String::new();
    for (i, ev) in report.events.iter().enumerate() {
        if i > 0 {
            buf.push('\n');
        }
        let status = ev.status.as_deref().unwrap_or("-");
        let _ = writeln!(buf, "{}  {}  [{}]", ev.timestamp, ev.event, status);
        let _ = writeln!(buf, "  trace    {}", ev.trace);
        match (&ev.target, &ev.target_to) {
            (Some(t), Some(to)) => {
                let _ = writeln!(buf, "  target   {t} → {to}");
            }
            (Some(t), None) => {
                let _ = writeln!(buf, "  target   {t}");
            }
            _ => {}
        }
        if !ev.body.is_empty() {
            let _ = writeln!(buf, "  body     {}", ev.body);
        }
        if status != "applied" {
            if let Some(code) = ev.attributes.get("reason_code").and_then(|v| v.as_str()) {
                let msg = ev
                    .attributes
                    .get("reason_message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let _ = writeln!(buf, "  reason   {code}: {msg}");
            }
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Map};

    fn event(status: &str) -> AuditEvent {
        let mut attributes = Map::new();
        attributes.insert("op_kind".into(), json!("move_document"));
        AuditEvent {
            timestamp: "2026-06-22T01:37:11.042Z".into(),
            trace: "abc123".into(),
            span: Some("span-1".into()),
            severity: "info".into(),
            event: "action.move_document".into(),
            body: "moved a.md → b.md".into(),
            status: Some(status.into()),
            target: Some("a.md".into()),
            target_to: Some("b.md".into()),
            attributes,
            raw: json!({"EventName": "norn.action.move_document"}),
        }
    }

    #[test]
    fn records_block_promotes_hot_fields_and_arrows_a_move() {
        let report = AuditReport {
            events: vec![event("applied")],
        };
        let out = audit_records(&report);
        assert!(out.contains("2026-06-22T01:37:11.042Z  action.move_document  [applied]"));
        assert!(out.contains("  trace    abc123"));
        assert!(out.contains("  target   a.md → b.md"));
        assert!(out.contains("  body     moved a.md → b.md"));
    }

    #[test]
    fn records_shows_reason_only_for_non_applied() {
        let mut ev = event("skipped");
        ev.attributes
            .insert("reason_code".into(), json!("precondition-unmet"));
        ev.attributes
            .insert("reason_message".into(), json!("hash mismatch"));
        let report = AuditReport { events: vec![ev] };
        let out = audit_records(&report);
        assert!(out.contains("  reason   precondition-unmet: hash mismatch"));
    }

    #[test]
    fn empty_records_is_empty_string() {
        assert_eq!(audit_records(&AuditReport::default()), "");
    }

    #[test]
    fn json_flattens_by_default_and_passes_raw_through() {
        let report = AuditReport {
            events: vec![event("applied")],
        };
        let flat = audit_json(&report, false);
        assert!(flat.starts_with('['));
        assert!(flat.contains("\"trace\":\"abc123\""));
        assert!(!flat.contains("EventName"), "flattened drops OTEL keys");
        let raw = audit_json(&report, true);
        assert!(raw.contains("EventName"));
    }

    #[test]
    fn json_empty_is_bracket_pair() {
        assert_eq!(audit_json(&AuditReport::default(), false), "[]");
    }
}
