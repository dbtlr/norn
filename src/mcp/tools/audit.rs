//! `vault.audit` — read the per-vault mutation event stream over MCP.

use crate::mcp::context::VaultContext;
use crate::telemetry::read::{self, Filter};
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct AuditParams {
    /// Only events from this invocation trace id.
    #[serde(default)]
    pub trace: Option<String>,
    /// Only per-action events with this status (`applied`/`skipped`/`failed`).
    #[serde(default)]
    pub status: Option<String>,
    /// Only events touching this vault-relative path (move source or dest).
    #[serde(default)]
    pub target: Option<String>,
    /// Lower time bound: `YYYY-MM-DD` or RFC-3339.
    #[serde(default)]
    pub since: Option<String>,
    /// Upper time bound: `YYYY-MM-DD` or RFC-3339.
    #[serde(default)]
    pub until: Option<String>,
    /// Max events, newest-first. Default 20.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Return stored OTEL objects verbatim instead of the flattened projection.
    #[serde(default)]
    pub raw: bool,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct AuditOutput {
    /// Matching events, newest-first. Flattened norn-native records unless
    /// `raw` was set (then the stored OTEL objects).
    pub events: Vec<serde_json::Value>,
}

pub fn handle_output(ctx: &VaultContext, p: AuditParams) -> Result<AuditOutput> {
    let since = match &p.since {
        Some(s) => Some(read::parse_since(s).map_err(|e| anyhow::anyhow!(e))?),
        None => None,
    };
    let until = match &p.until {
        Some(s) => Some(read::parse_until(s).map_err(|e| anyhow::anyhow!(e))?),
        None => None,
    };
    let filter = Filter {
        trace: p.trace,
        status: p.status,
        target: p.target,
        since,
        until,
    };
    let (_, events_dir) = crate::cache::events_dir_for(&ctx.vault_root)?;
    let limit = p.limit.unwrap_or(20);
    let stored = read::read_events(&events_dir, &filter, limit);
    let events = if p.raw {
        stored.iter().map(|e| e.raw().clone()).collect()
    } else {
        stored.iter().map(|e| e.flatten()).collect()
    };
    Ok(AuditOutput { events })
}
