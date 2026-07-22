//! Shared planner: converts intent (validation findings OR user-authored ops)
//! into a MigrationPlan that the applier can execute.
//!
//! Two intent sources:
//! - `findings`: the repair plan generator adapter (`plan_from_findings`), live
//!   via the `repair` verb.
//! - `intent`: the per-kind expanders for user-authored high-level ops, live via
//!   the applier.
//!
//! # Port note (ADR 0018)
//!
//! `intent::expand` is consumed by the pass-based executor (`crate::apply::executor`,
//! ported in this same NRN-386 task). `findings::plan_from_findings` is the
//! `repair` VERB's findings→plan adapter, consumed by `crate::read::repair`.

pub mod findings;
pub mod intent;
