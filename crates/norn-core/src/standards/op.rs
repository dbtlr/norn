//! The interior applier op model (ADR 0024, step 4b-2).
//!
//! [`ApplyOp`] is the flattened, typed working record the apply passes consume —
//! the successor to the former `PlannedChange`. It is built by TYPED construction
//! from [`norn_wire::TypedOp`] at the expansion boundary
//! ([`planner::intent::expand`](crate::planner::intent::expand)) and by the repair
//! planner ([`standards::repair`](crate::standards::repair)) when it materializes a
//! suggested plan. It is never serialized: the on-disk plan is the wire
//! [`MigrationPlan`](norn_wire::MigrationPlan) of typed ops; `ApplyOp` is a
//! purely-interior view the passes read by reference. Arbitrary-JSON leaves
//! (`expected_old_value` / `new_value` — a frontmatter value is any JSON) stay
//! [`Value`](serde_json::Value), matching ADR 0022's boundary; nothing else does.
//!
//! Finding linkage (`finding_code` / `repair_rule` / `finding_rule`) rides as
//! `Option<String>` — repair-sourced ops carry the real codes, verb-synthesized and
//! authored ops carry `None`. No `operator-request` sentinel is fabricated here; the
//! former interior default is gone. The applier never reads linkage to decide
//! behavior; the report echoes it as provenance from the authoritative wire op
//! (ADR 0022), so a structural op whose typed vocabulary
//! ([`MoveDocumentFields`](norn_wire::MoveDocumentFields) /
//! [`DeleteDocumentFields`](norn_wire::DeleteDocumentFields)) does not carry linkage
//! still echoes what it declared on the wire.

use camino::Utf8PathBuf;
use serde_json::Value;

use crate::standards::repair::link_risk::LinkRisk;
use crate::standards::repair::warnings::PlanWarning;
use crate::standards::RepairPlanSummary;

/// A single interior apply op — the flattened working record every pass and write
/// primitive reads. Built by typed construction from [`norn_wire::TypedOp`]; never
/// serialized.
#[derive(Debug, Clone)]
pub struct ApplyOp {
    pub change_id: String,
    pub path: Utf8PathBuf,
    pub document_hash: String,
    /// Finding-provenance linkage, `None` for verb-synthesized / authored ops.
    /// Never read by the applier; echoed onto the report from the wire op.
    pub finding_code: Option<String>,
    pub finding_rule: Option<String>,
    pub repair_rule: Option<String>,
    pub operation: String,
    pub field: Option<String>,
    pub expected_old_value: Option<Value>,
    pub new_value: Option<Value>,
    pub destination: Option<Utf8PathBuf>,
    pub(crate) link_risk: Option<LinkRisk>,
    pub warnings: Vec<PlanWarning>,
    /// When true, `apply_move` removes an existing destination before renaming.
    pub force: bool,
    /// When true, intermediate destination subdirectories are created at apply
    /// time (analogous to `mkdir -p`). Propagated from `move_folder` ops.
    pub parents: bool,
}

/// The minimal carrier the pass orchestrator
/// ([`run_apply_passes`](crate::apply::passes::run_apply_passes))
/// consumes — the successor to the deleted `RepairPlan`. Holds the `ApplyOp`
/// working set plus the few context fields the passes read: the schema version and
/// vault root the pre-write validation checks, and the planner's skip summary the
/// report echoes. No plan-shape fields (source filters, footnotes, rich skip
/// findings) ride here — those are the wire plan / planner-report's job.
#[derive(Debug, Clone)]
pub struct ApplyBatch {
    pub schema_version: u32,
    pub vault_root: Utf8PathBuf,
    pub summary: RepairPlanSummary,
    /// The interior working set. Keeps the `changes` vocabulary the passes read.
    pub changes: Vec<ApplyOp>,
}
