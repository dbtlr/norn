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
//! - [`graph`] — the vault walk + parse pipeline
//!   ([`graph::build_index_with_options`]) producing a resolved
//!   [`domain::GraphIndex`], plus the ignore-glob and alias-field machinery.
//! - [`links`] — the link model and resolution: Markdown-link and wikilink
//!   extraction into [`domain::Link`] records and matching a link to a document
//!   ([`links::resolve_links`]).
//! - [`query`] — the SQL-agnostic predicate model ([`query::DocumentQuery`]) and
//!   its input parsing ([`query::filter_args::build_document_query`],
//!   [`query::rule_scope_query`]). Shapes queries; the cache engine runs them.
//! - [`grammar`] — the ADR 0010 canonical-form + forgiving-input grammar:
//!   separator forgiveness, the query-family dynamic-predicate desugar
//!   ([`grammar::normalize_argv`]), and the field-universe gate. clap-free; the
//!   CLI injects its known-flag surface as a value.
//!
//! Deliberately NOT here yet (later port phases): the query/filter SQL emission
//! (the cache-engine run side) and the post-validation finding filters (blocked
//! on the validate engine's `Finding` model), the validate/repair engine and
//! apply verbs, and the cache engine — see `retired/CLAUDE.md`.

pub mod cache;
pub mod domain;
pub mod env;
pub mod grammar;
pub mod graph;
pub mod links;
pub mod query;
pub mod read;
pub mod standards;
pub mod target;

/// One-line boundary contract, referenced by every dependent so each
/// declared edge in the crate map is compiler-load-bearing.
pub const CONTRACT: &str = "norn-core: domain model, verb seam, plan/apply, validation, cache engine — value in, value out";

/// Direct-dependency contracts — the code reference that makes this
/// crate's declared edges load-bearing rather than manifest-only.
pub const DEP_CONTRACTS: &[&str] = &[norn_wire::CONTRACT, norn_frontmatter::CONTRACT];
