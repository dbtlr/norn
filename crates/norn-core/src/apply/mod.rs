//! The one mutation apply engine (ADR 0024).
//!
//! Every document-mutation source — the direct verbs (`set`/`new`/`edit`/`move`/
//! `delete`/`rewrite-wikilink`) and repair-as-planner — builds a `MigrationPlan`
//! and applies it through this one engine; there is no second applier. The
//! surface-neutral substrate:
//!
//! - [`executor`] — the one applier's plan-level orchestration: schema gate,
//!   op expansion, delete-hash + owner-set + `requires`-DAG validation, and
//!   assembling the [`ApplyReport`](norn_wire::ApplyReport) from the per-op
//!   outcomes the passes record. [`apply_migration_plan`] is the entry point.
//! - [`passes`] — the ordered named passes (content → delete → create → move →
//!   cascade → retry) that expand typed ops into file mutations, recording each
//!   op's outcome into a tracker AS IT HAPPENS. Partial apply is the semantics:
//!   an independent op still runs when a sibling fails.
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

pub mod envelope;
pub mod executor;
pub mod fsops;
pub mod passes;
pub mod preconditions;
pub mod transaction;

pub use executor::{apply_migration_plan, ApplyContext};
pub use preconditions::{build_owner_precondition_refusal_report, evaluate_owner_preconditions};
