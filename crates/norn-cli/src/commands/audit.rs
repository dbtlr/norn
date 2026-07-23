//! `norn audit` — read the per-vault mutation audit trail (the append-only
//! event stream every confirmed mutation appends to).
//!
//! The command maps its clap `Args` into an [`AuditParams`], summons the owner
//! (which reads the durable JSONL store co-located with the vault's state home
//! and returns the newest-first matches), and returns the [`AuditReport`] as an
//! [`Output`] for the display layer. `records` (default) is the vertical
//! key-value block per event; `json` is the flattened array (or the raw OTEL
//! passthrough with `--raw`).

use norn_wire::AuditParams;

use crate::cli::{AuditArgs, AuditFormat, AuditStatus, GlobalArgs};
use crate::display::{AuditView, Diagnostic, Format, FormatChoice, FormatSpec, Output};

impl From<AuditFormat> for Format {
    fn from(f: AuditFormat) -> Self {
        match f {
            // The audit records block is bespoke text rendered in the records
            // slot, unstyled (it never composes the record primitives).
            AuditFormat::Records => Format::Records,
            AuditFormat::Json => Format::Json,
        }
    }
}

impl AuditStatus {
    fn as_wire(self) -> String {
        match self {
            AuditStatus::Applied => "applied",
            AuditStatus::Skipped => "skipped",
            AuditStatus::Failed => "failed",
        }
        .to_string()
    }
}

/// Read the audit trail and return the report as an [`Output`] for the layer to
/// render, or a soft-landing [`Diagnostic`] on failure.
pub fn run(args: &AuditArgs, global: &GlobalArgs) -> Result<Output, Diagnostic> {
    let mut session = crate::routed::open_session(global)?;

    let params = AuditParams {
        trace: args.trace.clone(),
        status: args.status.map(AuditStatus::as_wire),
        target: args.target.clone(),
        since: args.since.clone(),
        until: args.until.clone(),
        limit: args.limit,
    };

    let report = session
        .audit(params)
        .map_err(|e| crate::routed::client_error_diagnostic(&e))?;

    Ok(Output::Audit(AuditView {
        report,
        raw: args.raw,
        format: FormatChoice {
            explicit: Some(Format::from(args.format)),
            spec: FormatSpec {
                tty: Format::Records,
                piped: Format::Records,
            },
        },
    }))
}
