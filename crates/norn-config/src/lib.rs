#![forbid(unsafe_code)]
//! The central config file (name -> { vault_root, vault_config, vault_cache, vault_logs, ... }), resolution order, reverse lookup, tier decision. The only crate that performs central-config IO.
//!
//! May never: Open caches, spawn processes, or serve.
//!
//! # What this crate owns
//!
//! norn's machine-local central config is the authoritative source of vault
//! identity (ADR 0017; ADR 0006 amendment 2026-07-17). It is a TOML file at
//! `<config_dir>/norn/config.toml` holding a `[vaults.<name>]` table per
//! registered vault. This crate is the sole performer of central-config IO:
//! the registry model, its durable read/modify/write, name↔root resolution,
//! and path reverse lookup.
//!
//! - [`ConfigHome`] — where the config file lives. Resolved from the
//!   environment ([`ConfigHome::from_env`]) or injected directly
//!   ([`ConfigHome::new`]) so tests never touch a real home directory.
//! - [`Registry`] — register / unregister / list / lookup / reverse-lookup,
//!   plus the [`Registry::resolve`] entry point implementing the full
//!   resolution order.
//! - [`ConfigError`] — a `thiserror` enum with operator-quality variants; no
//!   `anyhow` in the public API.
//!
//! # Resolution order
//!
//! [`Registry::resolve`] applies, highest precedence first: explicit path →
//! explicit name → repo binding (`.norn.toml`, walked up from the cwd) →
//! `NORN_ROOT` env → cwd reverse lookup. Any step that resolves *through the
//! registry* to a root that no longer exists fails loud
//! ([`ConfigError::StaleEntry`]) rather than silently falling through. A cwd
//! matching no registered vault yields [`ResolvedVia::UnregisteredCwd`] — not
//! an error; the caller decides what an unregistered vault means.
//!
//! # Deliberately out of scope (future responsibility)
//!
//! The **tier decision** (ephemeral / resident / managed, ADR 0017) is a
//! declared responsibility of this crate but lands with the summoner in a
//! later phase; it is not implemented here. Deriving default config / cache /
//! log locations from a vault root is likewise the caller's concern — the
//! registry stores only what was explicitly registered and never synthesizes
//! defaults.

/// One-line boundary contract, referenced by every dependent so each
/// declared edge in the crate map is compiler-load-bearing.
pub const CONTRACT: &str = "norn-config: central config, resolution order, reverse lookup, tier decision — the only crate performing central-config IO";

mod error;
mod home;
mod model;
mod registry;
mod resolve;

pub use error::ConfigError;
pub use home::ConfigHome;
pub use registry::{validate_name, RegisteredVault, Registry, VaultOverrides};
pub use resolve::{ResolveInput, Resolved, ResolvedVia, BINDING_FILENAME, NORN_ROOT_ENV};
