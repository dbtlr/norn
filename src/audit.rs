//! `norn audit` — read and render the per-vault mutation event stream.

use crate::cli::{AuditArgs, AuditFormat};
use crate::telemetry::read::{parse_since, parse_until, Filter, StoredEvent};

/// Build a reader `Filter` from CLI args; returns a user-facing error string
/// on an unparseable `--since`/`--until` (caller exits 2).
pub fn build_filter(args: &AuditArgs) -> Result<Filter, String> {
    let since = match &args.since {
        Some(s) => Some(parse_since(s)?),
        None => None,
    };
    let until = match &args.until {
        Some(s) => Some(parse_until(s)?),
        None => None,
    };
    Ok(Filter {
        trace: args.trace.clone(),
        status: args.status.map(|s| s.as_str().to_string()),
        target: args.target.clone(),
        since,
        until,
    })
}

/// Render events to the chosen format. `--raw` applies to JSON only.
pub fn render(events: &[StoredEvent], args: &AuditArgs) -> String {
    match args.format {
        AuditFormat::Json => {
            let array: Vec<serde_json::Value> = if args.raw {
                events.iter().map(|e| e.raw().clone()).collect()
            } else {
                events.iter().map(|e| e.flatten()).collect()
            };
            serde_json::to_string(&array).unwrap()
        }
        AuditFormat::Records => render_records(events),
    }
}

fn render_records(events: &[StoredEvent]) -> String {
    use std::fmt::Write as _;
    let mut buf = String::new();
    for (i, ev) in events.iter().enumerate() {
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
