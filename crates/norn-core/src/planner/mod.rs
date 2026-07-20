//! Shared planner: converts intent (validation findings OR user-authored ops)
//! into a MigrationPlan that the applier can execute.
//!
//! Two intent sources:
//! - `findings`: refactored home for today's repair plan generators (populated
//!   in Plan Task 17).
//! - `intent`: per-kind expanders for user-authored high-level ops (Plan Tasks
//!   4, 5, 6).
//!
//! # Port note (ADR 0018)
//!
//! `intent::expand` is consumed by the pass-based executor (`crate::apply`
//! executor, ported in this same NRN-386 task); `findings::plan_from_findings`
//! is the `repair` VERB's findings→plan adapter, which lands with that command.
//! Until both callers exist the entry points read as dead — allow it at the
//! module root; the applier port narrows this to just the findings adapter.
#![allow(dead_code)]

pub mod findings;
pub mod intent;
