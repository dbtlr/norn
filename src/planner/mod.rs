//! Shared planner: converts intent (validation findings OR user-authored ops)
//! into a MigrationPlan that the applier can execute.
//!
//! Two intent sources:
//! - `findings`: refactored home for today's repair plan generators (populated
//!   in Plan Task 17).
//! - `intent`: per-kind expanders for user-authored high-level ops (Plan Tasks
//!   4, 5, 6).

pub mod findings;
pub mod intent;
