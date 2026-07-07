//! `vault.audit` — read the per-vault mutation event stream over MCP.

use crate::mcp::context::VaultContext;
use crate::telemetry::read::{self, Filter};
use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Schema-carrying mirror of the CLI's `AuditStatus` (NRN-184). `cli::AuditStatus`
/// cannot derive `serde`/`schemars` — `cli.rs` is `#[path]`-included by `build.rs`,
/// whose build-script crate has neither dependency — so the MCP surface types its
/// `status` filter with this local enum instead. It lowers through
/// `cli::AuditStatus::as_str` (via [`AuditStatusFilter::to_cli`]), so the on-wire
/// status strings can never drift from the CLI's canonical mapping. The closed
/// variant set is what makes the published schema advertise `applied`/`skipped`/
/// `failed` and reject a typo as a params error.
#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AuditStatusFilter {
    Applied,
    Skipped,
    Failed,
}

impl AuditStatusFilter {
    fn to_cli(self) -> crate::cli::AuditStatus {
        match self {
            Self::Applied => crate::cli::AuditStatus::Applied,
            Self::Skipped => crate::cli::AuditStatus::Skipped,
            Self::Failed => crate::cli::AuditStatus::Failed,
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct AuditParams {
    /// Only events from this invocation trace id.
    #[serde(default)]
    pub trace: Option<String>,
    /// Only per-action events with this status. One of `applied`, `skipped`,
    /// `failed` — the same closed set `norn audit --status` enforces, so the
    /// published schema advertises the valid values and rejects typos.
    #[serde(default)]
    pub status: Option<AuditStatusFilter>,
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
        // Lower the closed enum to the wire string the event stream stores
        // (`applied`/`skipped`/`failed`) via the CLI's canonical mapping — the
        // same lowering `norn audit` does.
        status: p.status.map(|s| s.to_cli().as_str().to_string()),
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
