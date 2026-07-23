//! `vault.audit` — read the per-vault mutation audit trail.
//!
//! The param struct is a flat filter mirror (its schemars schema is the
//! published `inputSchema`); the handler maps it to a norn-wire [`AuditParams`]
//! and routes to the owner, then projects the [`AuditReport`] into the flat
//! [`AuditEnvelope`] — a `{ events: [...] }` object whose elements are the
//! flattened per-event projection, or (with `raw:true`) the stored OTEL Logs
//! objects verbatim. Capability-isomorphic with the `norn audit` CLI verb.

use norn_wire::{AuditEvent, AuditParams as WireAuditParams, AuditReport};
use serde::{Deserialize, Serialize};

/// The default newest-first cap, mirroring `norn audit`'s `--limit` default.
fn default_limit() -> usize {
    20
}

/// Parameters for `vault.audit` — mirrors `norn audit`'s flags. All filters are
/// AND-combined; an empty/absent stream yields `events: []`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuditParams {
    /// Match events whose trace id starts with this value (prefix match). One
    /// invocation per trace.
    #[serde(default)]
    pub trace: Option<String>,

    /// Keep only per-action events with this outcome (`applied` / `skipped` /
    /// `failed`). Lifecycle / planned events (no status) never match.
    #[serde(default)]
    pub status: Option<String>,

    /// Keep events whose mutation source OR destination path equals this value
    /// (useful for moves).
    #[serde(default)]
    pub target: Option<String>,

    /// Lower time bound. `YYYY-MM-DD` (UTC day start) or full RFC-3339. An
    /// unparseable value is a structured rejection (`isError: true`) — the
    /// same read-verb rejection convention every other read tool shares
    /// (the CLI's mirror of it is exit 1, never exit 2).
    #[serde(default)]
    pub since: Option<String>,

    /// Upper time bound. `YYYY-MM-DD` (UTC day end) or full RFC-3339.
    #[serde(default)]
    pub until: Option<String>,

    /// Maximum number of events to return, newest-first. Default 20.
    #[serde(default = "default_limit")]
    pub limit: usize,

    /// Return the stored OTEL Logs objects verbatim instead of the flattened
    /// projection.
    #[serde(default)]
    pub raw: bool,
}

/// Flat output envelope for `vault.audit`: a single `events` array so rmcp's
/// `type: object` root holds. Each element is the flattened projection, or the
/// raw OTEL object when `raw` was requested.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct AuditEnvelope {
    /// The matched events, newest-first, capped at `limit`.
    #[schemars(schema_with = "events_schema")]
    pub events: Vec<serde_json::Value>,
}

/// Typed schema for `events`: an array of objects (either the flattened
/// projection or the raw OTEL record).
fn events_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "array",
        "items": { "type": "object" }
    })
}

/// Build the wire request (the projection knob `raw` stays client-side).
pub(crate) fn to_wire(p: &AuditParams) -> WireAuditParams {
    WireAuditParams {
        trace: p.trace.clone(),
        status: p.status.clone(),
        target: p.target.clone(),
        since: p.since.clone(),
        until: p.until.clone(),
        limit: p.limit,
    }
}

/// Project the wire report into the flat output envelope.
pub(crate) fn envelope(report: AuditReport, raw: bool) -> AuditEnvelope {
    let events = if raw {
        report.events.into_iter().map(|e| e.raw).collect()
    } else {
        report.events.iter().map(AuditEvent::flatten).collect()
    };
    AuditEnvelope { events }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Map};

    fn event() -> AuditEvent {
        let mut attributes = Map::new();
        attributes.insert("op_kind".into(), json!("set"));
        AuditEvent {
            timestamp: "2026-06-22T01:00:00.000Z".into(),
            trace: "abc".into(),
            span: None,
            severity: "info".into(),
            event: "action.set".into(),
            body: "set x".into(),
            status: Some("applied".into()),
            target: Some("a.md".into()),
            target_to: None,
            attributes,
            raw: json!({"EventName": "norn.action.set"}),
        }
    }

    #[test]
    fn envelope_flattens_by_default() {
        let env = envelope(
            AuditReport {
                events: vec![event()],
            },
            false,
        );
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["events"][0]["trace"], "abc");
        assert!(v["events"][0].get("raw").is_none());
    }

    #[test]
    fn envelope_raw_passes_otel_through() {
        let env = envelope(
            AuditReport {
                events: vec![event()],
            },
            true,
        );
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["events"][0]["EventName"], "norn.action.set");
    }

    #[test]
    fn to_wire_carries_the_filters() {
        let p = AuditParams {
            trace: Some("abc".into()),
            status: Some("applied".into()),
            target: None,
            since: Some("2026-06-01".into()),
            until: None,
            limit: 5,
            raw: false,
        };
        let wire = to_wire(&p);
        assert_eq!(wire.trace.as_deref(), Some("abc"));
        assert_eq!(wire.limit, 5);
        assert_eq!(wire.since.as_deref(), Some("2026-06-01"));
    }
}
