//! The `audit` request (`Params`) and response (`Report`) vocabulary — the
//! read surface over the per-vault mutation event stream.
//!
//! Pure serde types. The owner reads the durable JSONL store (`events-*.jsonl`
//! under the registered vault's state/logs home), projects each stored OTEL
//! record into a flat [`AuditEvent`], applies the [`AuditParams`] filter, and
//! answers with an [`AuditReport`]. The CLI / MCP client renders it — the
//! `records` block, the flattened JSON array, or the raw OTEL passthrough are
//! all pure presentation transforms over the report.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// An `audit` request: the AND-combined filter over the event stream plus the
/// newest-first `limit`. `since`/`until` travel as raw strings (`YYYY-MM-DD`
/// or RFC-3339); the owner parses them, and an unparseable bound is a clean
/// rejection (exit 2 — bad filter argument), never a crash. `trace` is a
/// prefix match (one invocation per trace). `raw` selects the passthrough
/// projection client-side; the report always carries both the flat fields and
/// the untouched OTEL object, so it rides here only to document the request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuditParams {
    /// Match events whose trace id starts with this value (prefix match).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<String>,
    /// Keep only per-action events with this outcome (`applied` / `skipped` /
    /// `failed`). Lifecycle / planned events (no status) never match.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Keep events whose source OR destination path equals this value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Lower time bound (`YYYY-MM-DD` = UTC day start, or RFC-3339).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// Upper time bound (`YYYY-MM-DD` = UTC day end, or RFC-3339).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    /// Maximum number of events to return, newest-first. The CLI/MCP surface
    /// applies the default (20); the wire value is always explicit.
    pub limit: usize,
}

/// One mutation event read back from the stream, with the hot filter fields
/// promoted out of the OTEL `Attributes` bag. `raw` is the stored OTEL Logs
/// object, untouched, so a `--raw` render needs no second read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEvent {
    pub timestamp: String,
    pub trace: String,
    pub span: Option<String>,
    pub severity: String,
    /// Event name with the `norn.` prefix stripped (e.g. `action.move_document`).
    pub event: String,
    pub body: String,
    pub status: Option<String>,
    pub target: Option<String>,
    pub target_to: Option<String>,
    /// Remaining `norn.*` attributes: prefix stripped, dots → underscores, the
    /// three promoted keys (status / target / target.to) excluded.
    pub attributes: Map<String, Value>,
    /// The stored OTEL Logs object, verbatim (`--raw`).
    pub raw: Value,
}

impl AuditEvent {
    /// Flattened norn-native projection: hot fields at top level, the rest in a
    /// generic `attributes` bag, OTEL ceremony (Resource, capitalized keys,
    /// severity numbers) dropped. This is the `--format json` element shape.
    pub fn flatten(&self) -> Value {
        serde_json::json!({
            "timestamp": self.timestamp,
            "trace": self.trace,
            "span": self.span,
            "severity": self.severity,
            "event": self.event,
            "body": self.body,
            "status": self.status,
            "target": self.target,
            "target_to": self.target_to,
            "attributes": self.attributes,
        })
    }
}

/// An `audit` response: the matched events, newest-first, already capped at the
/// request's `limit`. An empty or absent stream yields an empty vec (exit 0).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AuditReport {
    pub events: Vec<AuditEvent>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> AuditEvent {
        let mut attributes = Map::new();
        attributes.insert("op_kind".into(), Value::String("move_document".into()));
        AuditEvent {
            timestamp: "2026-06-22T01:37:11.042Z".into(),
            trace: "trace-abc".into(),
            span: Some("span-1".into()),
            severity: "info".into(),
            event: "action.move_document".into(),
            body: "moved a.md → b.md".into(),
            status: Some("applied".into()),
            target: Some("a.md".into()),
            target_to: Some("b.md".into()),
            attributes,
            raw: serde_json::json!({"EventName": "norn.action.move_document"}),
        }
    }

    #[test]
    fn flatten_promotes_hot_fields_and_omits_raw() {
        let v = sample().flatten();
        assert_eq!(v["trace"], "trace-abc");
        assert_eq!(v["status"], "applied");
        assert_eq!(v["target_to"], "b.md");
        assert_eq!(v["event"], "action.move_document");
        assert_eq!(v["attributes"]["op_kind"], "move_document");
        assert!(v.get("raw").is_none(), "flatten drops the OTEL passthrough");
    }

    #[test]
    fn params_roundtrip_defaults_are_sparse() {
        let p = AuditParams {
            limit: 20,
            ..AuditParams::default()
        };
        let line = serde_json::to_string(&p).unwrap();
        assert_eq!(line, r#"{"limit":20}"#);
        assert_eq!(serde_json::from_str::<AuditParams>(&line).unwrap(), p);
    }
}
