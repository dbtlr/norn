//! `norn set <DOC>` command: schema-aware frontmatter mutation + wholesale
//! body replacement. Synthesizes a RepairPlan in-process and feeds it through
//! the existing apply_repair_plan orchestrator. Entry point is the
//! `Command::Set` dispatch arm in main.rs.

pub mod error;
pub mod report;
pub mod route;
pub mod synth;
pub mod validate;
