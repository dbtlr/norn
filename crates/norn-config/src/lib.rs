#![forbid(unsafe_code)]
//! The central config file (name -> { vault_root, vault_config, vault_cache, vault_logs, ... }), resolution order, reverse lookup, tier decision. The only crate that performs central-config IO.
//!
//! May never: Open caches, spawn processes, or serve.

/// One-line boundary contract, referenced by the bin so every edge in the
/// crate map is a real, compiler-checked dependency.
pub const CONTRACT: &str = "norn-config: The central config file (name -> { vault_root, vault_config, vault_cache, vault_logs, .";
