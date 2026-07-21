//! The mutation apply engine.
//!
//! This module owns the durable, surface-neutral substrate the mutation verbs
//! and the plan applier build on:
//!
//! - [`envelope`] — the engine-local error → coded [`norn_wire::ApplyError`]
//!   envelope conversions. The [`ApplyReport`](norn_wire::ApplyReport) output
//!   vocabulary, the [`ApplyOutcome`](norn_wire::ApplyOutcome) → exit-code
//!   mapping, and the coded error/warning envelope shapes are the end-user
//!   contract and live in norn-wire.
//! - [`preconditions`] — the ADR 0015 owner-set barrier: evaluate a plan's
//!   exact-owner-set preconditions against a fresh graph index before any write,
//!   and build the byte-identical-vault refusal report on a mismatch.
//!
//! The pass-based executor that expands typed ops into file mutations and drives
//! the cache write-through lands with the mutation verbs (`set`/`new`/`move`/
//! `delete`/`rewrite-wikilink`) that produce the plans it applies.

pub mod envelope;
pub mod executor;
pub mod preconditions;
pub mod repair_apply;

pub use executor::{apply_migration_plan, ApplyContext};
pub use preconditions::{build_owner_precondition_refusal_report, evaluate_owner_preconditions};
