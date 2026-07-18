#![forbid(unsafe_code)]
//! The Params/Report request/response vocabulary every side speaks — pure types.
//!
//! May never: Contain logic or IO, or depend on any other norn crate. Client-side crates depend on this instead of norn-core, which is what keeps "the client never opens a cache" compile-enforced.

/// One-line boundary contract, referenced by the bin so every edge in the
/// crate map is a real, compiler-checked dependency.
pub const CONTRACT: &str =
    "norn-wire: The Params/Report request/response vocabulary every side speaks — pure types.";
