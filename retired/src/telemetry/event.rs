//! Telemetry event data model, shaped after the OpenTelemetry Logs Data Model.
//!
//! An [`Event`] is a flat, self-contained record. [`Event::to_json`] renders it
//! to a single JSON object that follows the OTEL Logs shape (TraceId, SpanId,
//! SeverityNumber/Text, EventName, Body, Attributes, Resource).

use serde_json::{json, Map, Value};

/// Log severity, mapped to OpenTelemetry severity numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warn,
    Error,
}

impl Severity {
    /// OTEL SeverityNumber (INFO=9, WARN=13, ERROR=17).
    pub fn number(self) -> u8 {
        match self {
            Self::Info => 9,
            Self::Warn => 13,
            Self::Error => 17,
        }
    }

    /// OTEL SeverityText.
    pub fn text(self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}

/// A single telemetry event, self-contained for line-oriented (JSONL) output.
#[derive(Debug, Clone)]
pub struct Event {
    pub trace_id: String,
    pub span_id: Option<String>,
    pub severity: Severity,
    /// EventName, e.g. `"norn.action.move_document"`.
    pub name: String,
    /// Human-readable one-liner.
    pub body: String,
    pub attributes: Vec<(&'static str, String)>,
    /// RFC-3339 UTC, stamped at emit() time.
    pub timestamp: String,
}

impl Event {
    /// Serialize to the OTEL Logs Data Model shape (a flat self-contained JSON
    /// object). `SpanId` is `null` when there is no span.
    pub fn to_json(&self, service_version: &str) -> Value {
        let mut attrs = Map::new();
        for (k, v) in &self.attributes {
            attrs.insert((*k).to_string(), Value::String(v.clone()));
        }
        json!({
            "Timestamp": self.timestamp,
            "TraceId": self.trace_id,
            "SpanId": self.span_id,
            "SeverityNumber": self.severity.number(),
            "SeverityText": self.severity.text(),
            "EventName": self.name,
            "Body": self.body,
            "Attributes": Value::Object(attrs),
            "Resource": {
                "service.name": "norn",
                "service.version": service_version,
            },
        })
    }
}

// --- Event names -----------------------------------------------------------

/// Lifecycle: invocation started.
pub const EVENT_INVOCATION_STARTED: &str = "norn.invocation.started";
/// Lifecycle: invocation finished.
pub const EVENT_INVOCATION_FINISHED: &str = "norn.invocation.finished";
/// An operation was planned.
pub const EVENT_OP_PLANNED: &str = "norn.op.planned";
/// A retry round occurred.
pub const EVENT_RETRY: &str = "norn.retry";

/// Builds the per-action event name, e.g. `norn.action.move_document`.
pub fn action_event_name(kind: &str) -> String {
    format!("norn.action.{kind}")
}

// --- Attribute keys --------------------------------------------------------

pub const ATTR_OP_KIND: &str = "norn.op.kind";
pub const ATTR_TARGET: &str = "norn.target";
pub const ATTR_TARGET_TO: &str = "norn.target.to";
pub const ATTR_STATUS: &str = "norn.status";
pub const ATTR_REASON_CODE: &str = "norn.reason.code";
pub const ATTR_REASON_MESSAGE: &str = "norn.reason.message";
pub const ATTR_LINK_FROM: &str = "norn.link.from";
pub const ATTR_LINK_TO: &str = "norn.link.to";
pub const ATTR_OP_FROM: &str = "norn.op.from";
pub const ATTR_RETRY_ROUND: &str = "norn.retry.round";
pub const ATTR_DRY_RUN: &str = "norn.dry_run";
pub const ATTR_ARGV: &str = "norn.argv";
pub const ATTR_CWD: &str = "norn.cwd";
pub const ATTR_VAULT_ROOT: &str = "norn.vault_root";
pub const ATTR_EXIT: &str = "norn.exit";
pub const ATTR_TALLY_APPLIED: &str = "norn.tally.applied";
pub const ATTR_TALLY_SKIPPED: &str = "norn.tally.skipped";
pub const ATTR_TALLY_FAILED: &str = "norn.tally.failed";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_serializes_to_otel_shape() {
        let ev = Event {
            trace_id: "trace32".into(),
            span_id: Some("span16".into()),
            severity: Severity::Info,
            name: "norn.action.move_document".into(),
            body: "moved a.md → b.md".into(),
            attributes: vec![
                ("norn.op.kind", "move_document".into()),
                ("norn.status", "applied".into()),
            ],
            timestamp: "2026-05-29T14:03:11.042Z".into(),
        };
        let v = ev.to_json("0.34.0");
        assert_eq!(v["EventName"], "norn.action.move_document");
        assert_eq!(v["TraceId"], "trace32");
        assert_eq!(v["SpanId"], "span16");
        assert_eq!(v["SeverityNumber"], 9);
        assert_eq!(v["SeverityText"], "INFO");
        assert_eq!(v["Timestamp"], "2026-05-29T14:03:11.042Z");
        assert_eq!(v["Attributes"]["norn.status"], "applied");
        assert_eq!(v["Resource"]["service.name"], "norn");
        assert_eq!(v["Resource"]["service.version"], "0.34.0");
    }

    #[test]
    fn severity_numbers_match_otel() {
        assert_eq!(Severity::Info.number(), 9);
        assert_eq!(Severity::Warn.number(), 13);
        assert_eq!(Severity::Error.number(), 17);
        assert_eq!(Severity::Info.text(), "INFO");
    }
}
