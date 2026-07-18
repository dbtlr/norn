#![forbid(unsafe_code)]
//! The summoned daemon runtime: host loop, file watcher, drift auditor, continuous GC, idle-TTL self-reap, build-keyed socket bind. The only crate that opens cache databases.
//!
//! May never: Parse argv or resolve names for CWD sugar (client concerns).

/// One-line boundary contract, referenced by the bin so every edge in the
/// crate map is a real, compiler-checked dependency.
pub const CONTRACT: &str = "norn-owner: The summoned daemon runtime: host loop, file watcher, drift auditor, continuous GC, idle-TTL self-reap, build-keyed socket bind.";
