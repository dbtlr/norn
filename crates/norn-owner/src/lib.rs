#![forbid(unsafe_code)]
//! The summoned daemon runtime: host loop, file watcher, drift auditor, continuous GC, idle-TTL self-reap, build-keyed socket bind. The only crate that opens cache databases.
//!
//! May never: Parse argv or resolve names for CWD sugar (client concerns).

/// One-line boundary contract, referenced by every dependent so each
/// declared edge in the crate map is compiler-load-bearing.
pub const CONTRACT: &str =
    "norn-owner: the summoned daemon runtime — the only crate that opens cache databases";

/// Direct-dependency contracts — the code reference that makes this
/// crate's declared edges load-bearing rather than manifest-only.
pub const DEP_CONTRACTS: &[&str] = &[norn_core::CONTRACT, norn_wire::CONTRACT];
