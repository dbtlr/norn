//! Read-back over the per-vault mutation event stream (`events-*.jsonl`).
//! Mirror image of the write path in `super::store` / `super::event`.

use crate::telemetry::event::{ATTR_STATUS, ATTR_TARGET, ATTR_TARGET_TO};

/// One mutation event read back from the stream, with the hot filter fields
/// promoted out of the OTEL `Attributes` bag and the OTEL ceremony dropped.
#[derive(Debug, Clone)]
pub struct StoredEvent {
    pub timestamp: String,
    pub trace: String,
    pub span: Option<String>,
    pub severity: String,
    pub event: String,
    pub body: String,
    pub status: Option<String>,
    pub target: Option<String>,
    pub target_to: Option<String>,
    pub attributes: serde_json::Map<String, serde_json::Value>,
    pub raw: serde_json::Value,
}

/// Parse one JSONL line (an OTEL Logs object) into a `StoredEvent`.
/// Returns `None` for unparseable lines or lines missing required fields —
/// the caller skips them (best-effort, the writer owns the format).
pub fn parse_line(line: &str) -> Option<StoredEvent> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let obj = value.as_object()?;

    let timestamp = obj.get("Timestamp")?.as_str()?.to_string();
    let trace = obj.get("TraceId")?.as_str()?.to_string();
    let span = obj.get("SpanId").and_then(|s| s.as_str()).map(String::from);
    let severity = obj.get("SeverityText")?.as_str()?.to_ascii_lowercase();
    let event_name = obj.get("EventName")?.as_str()?;
    let event = event_name
        .strip_prefix("norn.")
        .unwrap_or(event_name)
        .to_string();
    let body = obj
        .get("Body")
        .and_then(|b| b.as_str())
        .unwrap_or_default()
        .to_string();

    let mut status = None;
    let mut target = None;
    let mut target_to = None;
    let mut attributes = serde_json::Map::new();
    if let Some(attrs) = obj.get("Attributes").and_then(|a| a.as_object()) {
        for (key, val) in attrs {
            match key.as_str() {
                ATTR_STATUS => status = val.as_str().map(String::from),
                ATTR_TARGET => target = val.as_str().map(String::from),
                ATTR_TARGET_TO => target_to = val.as_str().map(String::from),
                _ => {
                    let norn_key = key.strip_prefix("norn.").unwrap_or(key).replace('.', "_");
                    attributes.insert(norn_key, val.clone());
                }
            }
        }
    }

    Some(StoredEvent {
        timestamp,
        trace,
        span,
        severity,
        event,
        body,
        status,
        target,
        target_to,
        attributes,
        raw: value,
    })
}

impl StoredEvent {
    /// Flattened norn-native projection: hot fields top-level, the rest in
    /// a generic `attributes` bag. OTEL ceremony (Resource, capitalized keys,
    /// severity numbers) dropped.
    pub fn flatten(&self) -> serde_json::Value {
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

    /// The stored OTEL object, untouched (`--raw` / `raw:true`).
    pub fn raw(&self) -> &serde_json::Value {
        &self.raw
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_line() -> String {
        serde_json::json!({
            "Timestamp": "2026-06-22T01:37:11.042Z",
            "TraceId": "trace-abc",
            "SpanId": "span-1",
            "SeverityNumber": 9,
            "SeverityText": "INFO",
            "EventName": "norn.action.move_document",
            "Body": "moved a.md → b.md",
            "Attributes": {
                "norn.op.kind": "move_document",
                "norn.target": "a.md",
                "norn.target.to": "b.md",
                "norn.status": "applied"
            },
            "Resource": { "service.name": "norn", "service.version": "0.38.0" }
        })
        .to_string()
    }

    #[test]
    fn parse_line_extracts_promoted_fields_and_strips_prefixes() {
        let ev = parse_line(&sample_line()).expect("parses");
        assert_eq!(ev.timestamp, "2026-06-22T01:37:11.042Z");
        assert_eq!(ev.trace, "trace-abc");
        assert_eq!(ev.span.as_deref(), Some("span-1"));
        assert_eq!(ev.severity, "info"); // lowercased
        assert_eq!(ev.event, "action.move_document"); // "norn." stripped
        assert_eq!(ev.body, "moved a.md → b.md");
        assert_eq!(ev.status.as_deref(), Some("applied"));
        assert_eq!(ev.target.as_deref(), Some("a.md"));
        assert_eq!(ev.target_to.as_deref(), Some("b.md"));
        // remaining attrs: prefix stripped, dots → underscores, promoted three excluded
        assert_eq!(ev.attributes.get("op_kind").unwrap(), "move_document");
        assert!(ev.attributes.get("status").is_none());
        assert!(ev.attributes.get("target").is_none());
    }

    #[test]
    fn parse_line_rejects_garbage_and_missing_required() {
        assert!(parse_line("not json").is_none());
        assert!(parse_line(&serde_json::json!({"Body": "x"}).to_string()).is_none());
    }

    #[test]
    fn flatten_promotes_hot_fields_to_top_level() {
        let ev = parse_line(&sample_line()).unwrap();
        let v = ev.flatten();
        assert_eq!(v["trace"], "trace-abc");
        assert_eq!(v["status"], "applied");
        assert_eq!(v["target"], "a.md");
        assert_eq!(v["target_to"], "b.md");
        assert_eq!(v["event"], "action.move_document");
        assert_eq!(v["attributes"]["op_kind"], "move_document");
        assert!(v["attributes"].get("status").is_none());
    }

    #[test]
    fn flatten_nulls_absent_promoted_fields() {
        let line = serde_json::json!({
            "Timestamp": "2026-06-22T01:00:00.000Z", "TraceId": "t", "SpanId": null,
            "SeverityText": "INFO", "EventName": "norn.invocation.started", "Body": "started",
            "Attributes": { "norn.argv": "set note.md" },
            "Resource": {}
        })
        .to_string();
        let ev = parse_line(&line).unwrap();
        let v = ev.flatten();
        assert!(v["status"].is_null());
        assert!(v["target"].is_null());
        assert!(v["span"].is_null());
        assert_eq!(v["attributes"]["argv"], "set note.md");
    }

    #[test]
    fn raw_returns_stored_object_untouched() {
        let ev = parse_line(&sample_line()).unwrap();
        assert_eq!(ev.raw()["EventName"], "norn.action.move_document");
        assert_eq!(ev.raw()["Attributes"]["norn.status"], "applied");
    }
}
