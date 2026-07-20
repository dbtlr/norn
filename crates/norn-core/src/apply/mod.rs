//! The mutation apply engine.
//!
//! This module owns the durable, surface-neutral substrate the mutation verbs
//! and the plan applier build on:
//!
//! - [`report`] — the [`ApplyReport`] output vocabulary every mutation returns,
//!   the [`ApplyOutcome`] → exit-code mapping, and the coded error/warning
//!   envelope shapes.
//! - [`preconditions`] — the ADR 0015 owner-set barrier: evaluate a plan's
//!   exact-owner-set preconditions against a fresh graph index before any write,
//!   and build the byte-identical-vault refusal report on a mismatch.
//!
//! The pass-based executor that expands typed ops into file mutations and drives
//! the cache write-through lands with the mutation verbs (`set`/`new`/`move`/
//! `delete`/`rewrite-wikilink`) that produce the plans it applies.

pub mod preconditions;
pub mod repair_apply;
pub mod report;

pub use preconditions::{build_owner_precondition_refusal_report, evaluate_owner_preconditions};
pub use report::{
    ApplyError, ApplyOutcome, ApplyReport, ApplyReportOp, ApplyReportPrecondition, ApplyWarning,
    CascadeFailure, CascadeRewrite, CascadeSkip, CascadeSummary, LinkImpact, OpStatus,
    PreconditionStatus, APPLY_REPORT_SCHEMA_VERSION,
};
