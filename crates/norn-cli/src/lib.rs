#![forbid(unsafe_code)]
//! Thin CLI adapter: parse argv to Params, present Reports. One command-module pattern, one display-helper layer.
//!
//! May never: Contain verb logic or business logic beyond how the CLI itself works.

/// One-line boundary contract, referenced by every dependent so each
/// declared edge in the crate map is compiler-load-bearing.
pub const CONTRACT: &str = "norn-cli: thin CLI adapter — parse and present only";

/// Direct-dependency contracts — the code reference that makes this
/// crate's declared edges load-bearing rather than manifest-only.
pub const DEP_CONTRACTS: &[&str] = &[norn_client::CONTRACT, norn_wire::CONTRACT];
