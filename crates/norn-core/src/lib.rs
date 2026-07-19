#![forbid(unsafe_code)]
//! Domain model, verb seam (Params/execute/Report), plan/apply, validation, and the cache engine — value in, value out.
//!
//! May never: Touch sockets, clap, rmcp, the central config, process spawning, or ambient env/XDG/CWD resolution — all roots and paths arrive as values.
//!
//! # Surface (ported from the pre-rewrite tree, ADR 0018)
//!
//! - [`domain`] — the serializable graph vocabulary: [`domain::Document`],
//!   [`domain::GraphIndex`], the [`domain::Link`] model, and the diagnostic /
//!   heading / span types re-exported from `norn-frontmatter`. Pure data, no I/O.
//! - [`standards`] — the standards pack / rules model: the declarative
//!   [`standards::VaultConfig`] surface, YAML parse + compile
//!   ([`standards::parse_config`], [`standards::parse_config_compiled`]), path
//!   pattern matching, and the field-type / predicate declaration semantics.
//! - [`target`] — target resolution and backlink lookup over a built graph.
//! - [`env`] — the [`env::VaultEnv`] value-carrier: vault root plus injected
//!   config, value-in / value-out with no ambient reads.
//!
//! Deliberately NOT here yet (later port phases): the link *resolution* /
//! graph-build machinery, the query/filter layer, the validate/repair engine
//! and apply verbs, and the cache engine — see `retired/CLAUDE.md`.

pub mod domain;
pub mod env;
pub mod standards;
pub mod target;

/// One-line boundary contract, referenced by every dependent so each
/// declared edge in the crate map is compiler-load-bearing.
pub const CONTRACT: &str = "norn-core: domain model, verb seam, plan/apply, validation, cache engine — value in, value out";

/// Direct-dependency contracts — the code reference that makes this
/// crate's declared edges load-bearing rather than manifest-only.
pub const DEP_CONTRACTS: &[&str] = &[norn_wire::CONTRACT, norn_frontmatter::CONTRACT];
