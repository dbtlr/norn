#![forbid(unsafe_code)]
//! Domain model, verb seam (Params/execute/Report), plan/apply, validation, and the cache engine — value in, value out.
//!
//! May never: Touch sockets, clap, rmcp, the central config, process spawning, or ambient env/XDG/CWD resolution — all roots and paths arrive as values.

/// One-line boundary contract, referenced by the bin so every edge in the
/// crate map is a real, compiler-checked dependency.
pub const CONTRACT: &str = "norn-core: Domain model, verb seam (Params/execute/Report), plan/apply, validation, and the cache engine — value in, value out.";
