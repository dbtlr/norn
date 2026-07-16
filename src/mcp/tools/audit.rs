//! `vault.audit` — read the per-vault mutation event stream over MCP.

use crate::env::{RequestScope, VaultEnv};
use crate::telemetry::read::{self, Filter};
use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Schema-carrying mirror of the CLI's `AuditStatus` (NRN-184). `cli::AuditStatus`
/// cannot derive `serde`/`schemars` — `cli.rs` is `#[path]`-included by `build.rs`,
/// whose build-script crate has neither dependency — so the MCP surface types its
/// `status` filter with this local enum instead, and its closed variant set is
/// what makes the published schema advertise `applied`/`skipped`/`failed` and
/// reject a typo as a params error.
///
/// **What is guaranteed, and by which mechanism** (the mirror could otherwise
/// silently go stale against the CLI enum):
/// - **String values can't drift.** [`AuditStatusFilter::to_cli`] lowers each
///   mirror variant to a `cli::AuditStatus`, and the on-wire string comes from
///   `cli::AuditStatus::as_str` — the CLI's canonical mapping — never a literal
///   re-spelled here.
/// - **The variant set can't silently go stale.** [`AuditStatusFilter::from_cli`]
///   is an *exhaustive* match FROM `cli::AuditStatus`, so adding a 4th CLI variant
///   fails to compile until a mirror variant is added. The `mirror_is_bijective_
///   with_cli_audit_status` test closes the loop the other way: it iterates
///   clap's `value_variants()` and asserts every CLI variant has a mirror
///   deserializing from — and lowering back to — the same wire string.
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

    /// Compile-time drift guard (F2): an **exhaustive** match FROM every
    /// `cli::AuditStatus` variant. A new CLI variant makes this fail to compile
    /// until a mirror variant is added here — forcing the mirror to track the
    /// wire enum instead of quietly lagging it. Used by the bijection test.
    #[cfg(test)]
    fn from_cli(cli: crate::cli::AuditStatus) -> Self {
        match cli {
            crate::cli::AuditStatus::Applied => Self::Applied,
            crate::cli::AuditStatus::Skipped => Self::Skipped,
            crate::cli::AuditStatus::Failed => Self::Failed,
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

pub fn handle_output(ctx: &VaultEnv, _scope: &RequestScope, p: AuditParams) -> Result<AuditOutput> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::ValueEnum;

    /// F2 drift guard: the mirror enum is a bijection with `cli::AuditStatus`.
    /// Iterating clap's `value_variants()` asserts every CLI variant has a mirror
    /// that (a) deserializes from the CLI's wire string and (b) lowers back to
    /// that same string. Combined with the exhaustive `from_cli` match (which
    /// fails to compile if a CLI variant is added without a mirror), this makes a
    /// stale mirror impossible: a 4th CLI variant breaks compilation, and a
    /// mirror whose wire string diverged breaks this test.
    #[test]
    fn mirror_is_bijective_with_cli_audit_status() {
        for cli in crate::cli::AuditStatus::value_variants() {
            let possible = cli
                .to_possible_value()
                .expect("CLI audit status variant must have a clap value");
            let wire = possible.get_name();

            // Every CLI variant maps to a mirror (exhaustive `from_cli`), and that
            // mirror lowers back to the same canonical wire string.
            let mirror = AuditStatusFilter::from_cli(*cli);
            assert_eq!(
                mirror.to_cli().as_str(),
                wire,
                "mirror for CLI variant '{wire}' must lower to the same wire string"
            );

            // The mirror's serde/schema string equals the CLI's wire string:
            // deserializing the CLI's string must yield a mirror, or the published
            // enum schema has drifted from what `norn audit --status` accepts.
            let de: AuditStatusFilter = serde_json::from_value(serde_json::json!(wire))
                .unwrap_or_else(|e| {
                    panic!("CLI wire string '{wire}' must deserialize into the mirror enum: {e}")
                });
            assert_eq!(
                de.to_cli().as_str(),
                wire,
                "mirror deserialized from '{wire}' must round-trip to the same wire string"
            );
        }
    }
}
