#![forbid(unsafe_code)]
//! Thin CLI adapter: parse argv to Params, present Reports. One command-module pattern, one display-helper layer.
//!
//! May never: Contain verb logic or business logic beyond how the CLI itself works.

/// One-line boundary contract, referenced by the bin so every edge in the
/// crate map is a real, compiler-checked dependency.
pub const CONTRACT: &str = "norn-cli: Thin CLI adapter: parse argv to Params, present Reports.";
