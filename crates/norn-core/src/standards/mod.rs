//! The standards pack: the declarative rules model.
//!
//! Where a vault's standards are *declared* and *compiled* — `config` parses and
//! validates the `VaultConfig` (validate rules, repair rules, retention, index
//! policy) and compiles its path patterns; `path_match` is the pattern-matching
//! and `effective_match_glob` machinery those rules lower to; `predicates` holds
//! the field-type and selector declaration semantics the compiler conflict-checks
//! against; `duration` parses the short duration strings the config carries.
//!
//! # Ported seam (ADR 0018)
//!
//! Beyond the declaration side, this module also carries the standards remnant
//! shared by mutation and validate (NRN-376): `substitution` renders the
//! `{{…}}` templates, `defaults` resolves `frontmatter_defaults` to a fixpoint
//! (`applicable_rules` / `merge_defaults` / `resolve_to_fixpoint`, clock
//! injected value-in), and `predicates` carries the document-matching helpers.
//! `template_refs` holds the config-load `{{…}}` reference-scanning and the
//! `KNOWN_TRANSFORMS` declaration list, pinned equal to the `substitution`
//! renderer's transform table. Still deferred to the phase-3 mutation port: the
//! validate engine, findings, repair planning, and the verb-level apply
//! machinery (the minimal-edit splice core already went to
//! `norn-frontmatter::edit`).

pub mod config;
pub mod defaults;
pub mod duration;
mod index_policy;
pub mod path_match;
pub mod predicates;
pub mod substitution;
mod template_refs;

pub use index_policy::resolved_index_set;

pub use config::{
    parse_config, parse_config_compiled, CacheConfig, CompiledConfig, CompiledRule, ConfigError,
    FieldReferenceConstraint, FieldTypeDecl, FieldTypeSpec, RepairAction, RepairConfig, RepairRule,
    ValidateConfig, ValidateRule, VaultConfig, CURRENT_SCHEMA_VERSION, DEFAULT_CACHE_RETENTION,
    DEFAULT_RETENTION, DEFAULT_STRING_MAX_LENGTH, STRING_MAX_LENGTH_CEILING,
};
pub use defaults::{
    applicable_rules, merge_defaults, path_variables, resolve_to_fixpoint, ResolveError,
};
pub use duration::parse_duration;
pub use path_match::{effective_match_glob, glob_from_target, pattern_from_target, PathPattern};
pub use substitution::{format_datetime, render, Context, RenderError};
