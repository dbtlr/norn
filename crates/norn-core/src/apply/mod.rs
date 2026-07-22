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
//! - [`fsops`] — the narrow, named filesystem write primitives (atomic
//!   durable write, move, delete, create-document materialization, and the
//!   vault-root containment gate). Every disk effect the passes cause lands
//!   through one of these.
//! - [`transaction`] — the per-file fingerprint → shadow → verify → swap unit:
//!   a file-bytes CAS and a swap re-read that catch external modification the
//!   old two-phase applier could miss.
//!
//! The pass-based executor that expands typed ops into file mutations and drives
//! the cache write-through lands with the mutation verbs (`set`/`new`/`move`/
//! `delete`/`rewrite-wikilink`) that produce the plans it applies.

pub mod envelope;
pub mod executor;
pub mod fsops;
pub mod preconditions;
pub mod repair_apply;
pub mod transaction;

pub use executor::{apply_migration_plan, ApplyContext};
pub use preconditions::{build_owner_precondition_refusal_report, evaluate_owner_preconditions};
