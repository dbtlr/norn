#![forbid(unsafe_code)]
//! Domain model, verb seam (Params/execute/Report), plan/apply, validation, and the cache engine — value in, value out.
//!
//! May never: Touch sockets, clap, rmcp, the central config, process spawning, or ambient env/XDG/CWD resolution — all roots and paths arrive as values.

/// One-line boundary contract, referenced by every dependent so each
/// declared edge in the crate map is compiler-load-bearing.
pub const CONTRACT: &str = "norn-core: domain model, verb seam, plan/apply, validation, cache engine — value in, value out";

/// Direct-dependency contracts — the code reference that makes this
/// crate's declared edges load-bearing rather than manifest-only.
pub const DEP_CONTRACTS: &[&str] = &[norn_wire::CONTRACT, norn_frontmatter::CONTRACT];
