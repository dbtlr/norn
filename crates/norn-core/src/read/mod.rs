//! The read verbs' execute seams (ADR 0016 Params/execute/Report).
//!
//! Each verb is a pure function of a warm [`Cache`](crate::cache::Cache) plus a
//! wire `Params`, producing a wire `Report`. The owner drives these inside
//! `serve_read`; the CLI renders the returned `Report`. No IO beyond the cache
//! read, no clock read (the current date is injected as `today`).
//!
//! Shipped this phase: [`find`] and [`count`]. `get` / `describe` follow.

pub mod count;
pub mod find;
