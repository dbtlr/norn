#![forbid(unsafe_code)]
//! The Params/Report request/response vocabulary every side speaks — pure types.
//!
//! May never: Contain logic or IO, or depend on any other norn crate. Client-side crates depend on this instead of norn-core, which is what keeps "the client never opens a cache" compile-enforced.

/// One-line boundary contract, referenced by every dependent so each
/// declared edge in the crate map is compiler-load-bearing.
pub const CONTRACT: &str = "norn-wire: the Params/Report vocabulary — pure types, no logic or IO";
