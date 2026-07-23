//! Read-back over the per-vault mutation event stream (`events-*.jsonl`) — the
//! mirror image of the write path in [`super::store`] / [`super::event`].
//!
//! Produces [`norn_wire::AuditEvent`] rows directly (the wire vocabulary the
//! `audit` verb answers with), so the projection lives in one place. The events
//! dir arrives as a value — norn-core reads the files, never resolves the root.

use camino::Utf8Path;

use crate::telemetry::event::{ATTR_STATUS, ATTR_TARGET, ATTR_TARGET_TO};
use norn_wire::AuditEvent;

/// Parse one JSONL line (an OTEL Logs object) into an [`AuditEvent`], with the
/// hot filter fields promoted out of the `Attributes` bag. Returns `None` for
/// unparseable lines or lines missing required fields — the caller skips them
/// (best-effort; the writer owns the format).
pub fn parse_line(line: &str) -> Option<AuditEvent> {
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

    Some(AuditEvent {
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

/// AND-combined filter over the event stream. `None` fields are no-ops. `trace`
/// is a PREFIX match (one invocation per trace); the timestamp bounds are
/// inclusive.
#[derive(Debug, Default, Clone)]
pub struct Filter {
    pub trace: Option<String>,
    pub status: Option<String>,
    pub target: Option<String>,
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    pub until: Option<chrono::DateTime<chrono::Utc>>,
}

impl Filter {
    pub fn matches(&self, ev: &AuditEvent) -> bool {
        if let Some(t) = &self.trace {
            if !ev.trace.starts_with(t) {
                return false;
            }
        }
        if let Some(s) = &self.status {
            // Events without a status (lifecycle / op.planned) never match a
            // status filter.
            if ev.status.as_deref() != Some(s.as_str()) {
                return false;
            }
        }
        if let Some(tg) = &self.target {
            let hit = ev.target.as_deref() == Some(tg.as_str())
                || ev.target_to.as_deref() == Some(tg.as_str());
            if !hit {
                return false;
            }
        }
        if self.since.is_some() || self.until.is_some() {
            let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(&ev.timestamp) else {
                return false;
            };
            let ts = parsed.with_timezone(&chrono::Utc);
            if let Some(s) = self.since {
                if ts < s {
                    return false;
                }
            }
            if let Some(u) = self.until {
                if ts > u {
                    return false;
                }
            }
        }
        true
    }
}

/// Parse an `events-YYYY-MM-DD.jsonl` filename to its date.
fn event_file_date(name: &str) -> Option<chrono::NaiveDate> {
    let date = name.strip_prefix("events-")?.strip_suffix(".jsonl")?;
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}

/// Read up to `limit` matching events, newest-first, across all daily files in
/// `dir`. Missing dir → empty. Best-effort: an unreadable file / unparseable
/// line is skipped. Stops early once `limit` is met or files predate `since`.
pub fn read_events(dir: &Utf8Path, filter: &Filter, limit: usize) -> Vec<AuditEvent> {
    let mut out = Vec::new();
    if limit == 0 {
        return out;
    }
    let Ok(entries) = std::fs::read_dir(dir.as_std_path()) else {
        return out;
    };
    let mut files: Vec<(chrono::NaiveDate, std::path::PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(date) = event_file_date(name) else {
            continue;
        };
        files.push((date, entry.path()));
    }
    files.sort_by_key(|f| std::cmp::Reverse(f.0)); // date descending (newest file first)

    let since_date = filter.since.map(|s| s.date_naive());
    for (date, path) in files {
        if let Some(sd) = since_date {
            if date < sd {
                break; // no older file can hold an event >= since
            }
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in content.lines().rev() {
            // within a file lines are chronological; rev() → newest-first
            if line.trim().is_empty() {
                continue;
            }
            let Some(ev) = parse_line(line) else { continue };
            if !filter.matches(&ev) {
                continue;
            }
            out.push(ev);
            if out.len() >= limit {
                return out;
            }
        }
    }
    out
}

/// Lower time bound: bare `YYYY-MM-DD` → start of that UTC day; else RFC-3339.
pub fn parse_since(s: &str) -> Result<chrono::DateTime<chrono::Utc>, String> {
    parse_bound(s, false)
}

/// Upper time bound: bare `YYYY-MM-DD` → end of that UTC day; else RFC-3339.
pub fn parse_until(s: &str) -> Result<chrono::DateTime<chrono::Utc>, String> {
    parse_bound(s, true)
}

fn parse_bound(s: &str, end_of_day: bool) -> Result<chrono::DateTime<chrono::Utc>, String> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&chrono::Utc));
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let time = if end_of_day {
            chrono::NaiveTime::from_hms_milli_opt(23, 59, 59, 999).unwrap()
        } else {
            chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap()
        };
        let naive = date.and_time(time);
        return Ok(chrono::DateTime::from_naive_utc_and_offset(
            naive,
            chrono::Utc,
        ));
    }
    Err(format!(
        "invalid date/time '{s}' (expected YYYY-MM-DD or RFC-3339)"
    ))
}

/// Reject a reversed `--since`/`--until` range. A `since` strictly after
/// `until` can never match an event, so letting it through would silently
/// return an empty report — indistinguishable from "no events in range".
/// This is a clean rejection (the same read-verb `Rejected` convention as an
/// unparseable date), not an empty result. `since == until` (a single-instant
/// range) and either bound absent both pass through unchanged.
pub fn validate_bounds(
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<(), String> {
    if let (Some(s), Some(u)) = (since, until) {
        if s > u {
            return Err("since is after until".to_string());
        }
    }
    Ok(())
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
    fn filter_trace_is_a_prefix_match() {
        let ev = parse_line(&sample_line()).unwrap();
        let f = Filter {
            trace: Some("trace-".into()),
            ..Filter::default()
        };
        assert!(f.matches(&ev), "prefix matches");
        let miss = Filter {
            trace: Some("nope".into()),
            ..Filter::default()
        };
        assert!(!miss.matches(&ev));
    }

    #[test]
    fn filter_status_excludes_events_without_a_status() {
        let line = serde_json::json!({
            "Timestamp": "2026-06-22T01:00:00.000Z", "TraceId": "t", "SpanId": null,
            "SeverityText": "INFO", "EventName": "norn.op.planned", "Body": "planned",
            "Attributes": {}, "Resource": {}
        })
        .to_string();
        let ev = parse_line(&line).unwrap();
        let f = Filter {
            status: Some("applied".into()),
            ..Filter::default()
        };
        assert!(
            !f.matches(&ev),
            "a status-less event never matches a status filter"
        );
    }

    #[test]
    fn read_events_newest_first_respects_limit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let mk = |ts: &str, body: &str| {
            serde_json::json!({
                "Timestamp": ts, "TraceId": "t", "SpanId": null,
                "SeverityText": "INFO", "EventName": "norn.action.set",
                "Body": body, "Attributes": {"norn.status": "applied"}, "Resource": {}
            })
            .to_string()
        };
        std::fs::write(
            tmp.path().join("events-2026-06-22.jsonl"),
            format!(
                "{}\n{}\n",
                mk("2026-06-22T01:00:00.000Z", "first"),
                mk("2026-06-22T02:00:00.000Z", "second")
            ),
        )
        .unwrap();
        let events = read_events(dir, &Filter::default(), 1);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].body, "second", "newest-first within a file");
    }

    #[test]
    fn read_events_missing_dir_is_empty() {
        let dir = camino::Utf8Path::new("/nonexistent-events-dir-xyz");
        assert!(read_events(dir, &Filter::default(), 20).is_empty());
    }

    #[test]
    fn parse_bounds_accept_date_and_rfc3339() {
        let since = parse_since("2026-06-01").unwrap();
        assert_eq!(since.to_rfc3339(), "2026-06-01T00:00:00+00:00");
        let until = parse_until("2026-06-01").unwrap();
        assert!(until.to_rfc3339().starts_with("2026-06-01T23:59:59"));
        assert!(parse_since("garbage").is_err());
        assert!(parse_since("2026-06-22T14:00:00Z").is_ok());
    }

    #[test]
    fn validate_bounds_rejects_since_after_until() {
        let since = parse_since("2026-06-10").unwrap();
        let until = parse_until("2026-06-01").unwrap();
        let err = validate_bounds(Some(since), Some(until))
            .expect_err("since after until must be rejected");
        assert_eq!(err, "since is after until");
    }

    #[test]
    fn validate_bounds_accepts_since_before_or_equal_until() {
        let since = parse_since("2026-06-01").unwrap();
        let until = parse_until("2026-06-10").unwrap();
        assert!(validate_bounds(Some(since), Some(until)).is_ok());

        // The same instant for both bounds is a valid single-point range.
        let same = parse_since("2026-06-01").unwrap();
        assert!(validate_bounds(Some(same), Some(same)).is_ok());
    }

    #[test]
    fn validate_bounds_passes_when_either_bound_is_absent() {
        let one = parse_since("2026-06-01").unwrap();
        assert!(validate_bounds(Some(one), None).is_ok());
        assert!(validate_bounds(None, Some(one)).is_ok());
        assert!(validate_bounds(None, None).is_ok());
    }
}
