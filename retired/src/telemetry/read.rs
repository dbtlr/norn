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

use camino::Utf8Path;

/// AND-combined filter over the event stream. `None` fields are no-ops.
#[derive(Debug, Default, Clone)]
pub struct Filter {
    pub trace: Option<String>,
    pub status: Option<String>,
    pub target: Option<String>,
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    pub until: Option<chrono::DateTime<chrono::Utc>>,
}

impl Filter {
    pub fn matches(&self, ev: &StoredEvent) -> bool {
        if let Some(t) = &self.trace {
            if &ev.trace != t {
                return false;
            }
        }
        if let Some(s) = &self.status {
            // Events without a status (lifecycle / op.planned) never match a status filter.
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

/// Parse an `events-YYYY-MM-DD.jsonl` filename to its date. (Mirrors the
/// write-side helper in `super::store`; kept local so the reader is
/// self-contained.)
fn event_file_date(name: &str) -> Option<chrono::NaiveDate> {
    let date = name.strip_prefix("events-")?.strip_suffix(".jsonl")?;
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}

/// Read up to `limit` matching events, newest-first, across all daily files
/// in `dir`. Missing dir → empty. Best-effort: unreadable file / unparseable
/// line skipped. Stops early once `limit` is met or files predate `since`.
pub fn read_events(dir: &Utf8Path, filter: &Filter, limit: usize) -> Vec<StoredEvent> {
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

    use camino::Utf8Path;

    fn write_file(dir: &Utf8Path, name: &str, lines: &[&str]) {
        std::fs::write(dir.join(name), format!("{}\n", lines.join("\n"))).unwrap();
    }

    fn ev_line(ts: &str, trace: &str, event: &str, status: &str, target: &str) -> String {
        serde_json::json!({
            "Timestamp": ts, "TraceId": trace, "SpanId": "s",
            "SeverityText": "INFO", "EventName": event, "Body": "b",
            "Attributes": { "norn.status": status, "norn.target": target },
            "Resource": {}
        })
        .to_string()
    }

    #[test]
    fn read_events_orders_newest_first_across_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();
        write_file(
            dir,
            "events-2026-06-20.jsonl",
            &[&ev_line(
                "2026-06-20T10:00:00.000Z",
                "t1",
                "norn.action.set",
                "applied",
                "a.md",
            )],
        );
        write_file(
            dir,
            "events-2026-06-21.jsonl",
            &[
                &ev_line(
                    "2026-06-21T09:00:00.000Z",
                    "t2",
                    "norn.action.set",
                    "applied",
                    "b.md",
                ),
                &ev_line(
                    "2026-06-21T11:00:00.000Z",
                    "t3",
                    "norn.action.set",
                    "applied",
                    "c.md",
                ),
            ],
        );
        let got = read_events(dir, &Filter::default(), 100);
        let traces: Vec<&str> = got.iter().map(|e| e.trace.as_str()).collect();
        assert_eq!(traces, vec!["t3", "t2", "t1"]); // strictly newest-first
    }

    #[test]
    fn read_events_respects_limit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();
        write_file(
            dir,
            "events-2026-06-21.jsonl",
            &[
                &ev_line(
                    "2026-06-21T09:00:00.000Z",
                    "t1",
                    "norn.action.set",
                    "applied",
                    "a.md",
                ),
                &ev_line(
                    "2026-06-21T10:00:00.000Z",
                    "t2",
                    "norn.action.set",
                    "applied",
                    "b.md",
                ),
                &ev_line(
                    "2026-06-21T11:00:00.000Z",
                    "t3",
                    "norn.action.set",
                    "applied",
                    "c.md",
                ),
            ],
        );
        let got = read_events(dir, &Filter::default(), 2);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].trace, "t3"); // newest two
        assert_eq!(got[1].trace, "t2");
    }

    #[test]
    fn read_events_missing_dir_is_empty_not_error() {
        let dir = Utf8Path::new("/no/such/events/dir/xyz");
        assert!(read_events(dir, &Filter::default(), 10).is_empty());
    }

    #[test]
    fn read_events_skips_unparseable_lines() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();
        write_file(
            dir,
            "events-2026-06-21.jsonl",
            &[
                "garbage not json",
                &ev_line(
                    "2026-06-21T09:00:00.000Z",
                    "t1",
                    "norn.action.set",
                    "applied",
                    "a.md",
                ),
            ],
        );
        let got = read_events(dir, &Filter::default(), 10);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].trace, "t1");
    }

    #[test]
    fn filter_by_trace_status_target() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();
        write_file(
            dir,
            "events-2026-06-21.jsonl",
            &[
                &ev_line(
                    "2026-06-21T09:00:00.000Z",
                    "t1",
                    "norn.action.set",
                    "applied",
                    "a.md",
                ),
                &ev_line(
                    "2026-06-21T10:00:00.000Z",
                    "t2",
                    "norn.action.set",
                    "skipped",
                    "b.md",
                ),
            ],
        );
        let by_trace = read_events(
            dir,
            &Filter {
                trace: Some("t1".into()),
                ..Default::default()
            },
            10,
        );
        assert_eq!(by_trace.len(), 1);
        assert_eq!(by_trace[0].trace, "t1");

        let by_status = read_events(
            dir,
            &Filter {
                status: Some("skipped".into()),
                ..Default::default()
            },
            10,
        );
        assert_eq!(by_status.len(), 1);
        assert_eq!(by_status[0].trace, "t2");

        let by_target = read_events(
            dir,
            &Filter {
                target: Some("b.md".into()),
                ..Default::default()
            },
            10,
        );
        assert_eq!(by_target.len(), 1);
        assert_eq!(by_target[0].trace, "t2");
    }

    #[test]
    fn filter_target_matches_either_endpoint_of_a_move() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();
        let mv = serde_json::json!({
            "Timestamp": "2026-06-21T09:00:00.000Z", "TraceId": "t1", "SpanId": "s",
            "SeverityText": "INFO", "EventName": "norn.action.move_document", "Body": "b",
            "Attributes": { "norn.status": "applied", "norn.target": "from.md", "norn.target.to": "to.md" },
            "Resource": {}
        }).to_string();
        write_file(dir, "events-2026-06-21.jsonl", &[&mv]);
        assert_eq!(
            read_events(
                dir,
                &Filter {
                    target: Some("from.md".into()),
                    ..Default::default()
                },
                10
            )
            .len(),
            1
        );
        assert_eq!(
            read_events(
                dir,
                &Filter {
                    target: Some("to.md".into()),
                    ..Default::default()
                },
                10
            )
            .len(),
            1
        );
    }

    #[test]
    fn filter_since_until_bounds_and_early_stop() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();
        write_file(
            dir,
            "events-2026-06-19.jsonl",
            &[&ev_line(
                "2026-06-19T09:00:00.000Z",
                "old",
                "norn.action.set",
                "applied",
                "a.md",
            )],
        );
        write_file(
            dir,
            "events-2026-06-21.jsonl",
            &[&ev_line(
                "2026-06-21T09:00:00.000Z",
                "new",
                "norn.action.set",
                "applied",
                "b.md",
            )],
        );
        let since = parse_since("2026-06-20").unwrap();
        let got = read_events(
            dir,
            &Filter {
                since: Some(since),
                ..Default::default()
            },
            10,
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].trace, "new");

        let until = parse_until("2026-06-20").unwrap();
        let got = read_events(
            dir,
            &Filter {
                until: Some(until),
                ..Default::default()
            },
            10,
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].trace, "old");
    }

    #[test]
    fn parse_bounds_accepts_date_and_rfc3339_and_rejects_garbage() {
        assert!(parse_since("2026-06-22").is_ok());
        assert!(parse_until("2026-06-22T12:00:00Z").is_ok());
        assert!(parse_since("nope").is_err());
    }
}
