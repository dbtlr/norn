//! Shared test-support helpers. The single home for the skip-if-absent
//! oracle probe consumed by this crate's and `norn-parity`'s test suites.
//!
//! This is the one place in `norn-fixtures` that shells out to `norn`: the
//! generator itself never does (it produces inputs independent of the system
//! under test). The probe lives here only so both crates' tests share one
//! copy rather than re-deriving it.

use std::process::Command;

/// `true` when the pinned oracle (`norn`, ADR 0018) is on PATH and its
/// `--version` succeeds. Oracle-touching tests skip cleanly when it is
/// absent (it is installed before `cargo test` in CI).
pub fn oracle_present() -> bool {
    Command::new("norn")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
