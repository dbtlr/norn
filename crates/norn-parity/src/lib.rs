#![forbid(unsafe_code)]
//! ADR 0018 oracle parity harness: drive the pinned oracle binary and the
//! rewrite binary with identical argv over generated fixture vaults,
//! compare their outputs under normalization, and gate on exactly three
//! verdicts — match, diverged-with-ledger-entry (passes, citing the
//! entry), drift (fails). No fourth state: a case that cannot run at all
//! (missing binary, fixture generation failure, a signaled process) is a
//! runner error, not a verdict.
//!
//! See `docs/decisions/0018-greenfield-rewrite-oracle-parity.md` for the
//! mechanics this crate implements, and `docs/parity-ledger.toml` for the
//! decision-gated divergence ledger it enforces.

pub mod cases;
pub mod consistency;
pub mod exec;
pub mod fixtures;
pub mod ledger;
pub mod normalize;
pub mod paths;
pub mod poststate;
pub mod report;
pub mod run;
pub mod verdict;

pub use cases::{suites, Case, Fixture, Suite};
pub use ledger::{Entry as LedgerEntry, Ledger, LedgerError, Reason as LedgerReason};
pub use run::{Mode, RunConfig, RunError, RunReport};
pub use verdict::Verdict;
