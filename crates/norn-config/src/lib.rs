#![forbid(unsafe_code)]
//! The central config file (name -> { vault_root, vault_config, vault_cache, vault_logs, ... }), resolution order, reverse lookup, tier decision. The only crate that performs central-config IO.
//!
//! May never: Open caches, spawn processes, or serve.

/// One-line boundary contract, referenced by every dependent so each
/// declared edge in the crate map is compiler-load-bearing.
pub const CONTRACT: &str = "norn-config: central config, resolution order, reverse lookup, tier decision — the only crate performing central-config IO";
