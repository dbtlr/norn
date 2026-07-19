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
//! This is the declaration side only. The verb-level machinery that *runs*
//! declared standards over a vault — the validate engine, findings, repair
//! planning, the substitution renderer, and the minimal-edit apply primitives
//! (whose splice core already went to `norn-frontmatter::edit`) — is the
//! phase-3 mutation port and lives elsewhere. `template_refs` carries only the
//! `{{…}}` reference-scanning the config checks need; the renderer that
//! consumes those references is deferred with the rest of the engine.

pub mod config;
pub mod duration;
mod index_policy;
pub mod path_match;
pub mod predicates;
mod template_refs;

pub use index_policy::resolved_index_set;

pub use config::{
    parse_config, parse_config_compiled, CacheConfig, CompiledConfig, CompiledRule, ConfigError,
    FieldReferenceConstraint, FieldTypeDecl, FieldTypeSpec, RepairAction, RepairConfig, RepairRule,
    ValidateConfig, ValidateRule, VaultConfig, CURRENT_SCHEMA_VERSION, DEFAULT_CACHE_RETENTION,
    DEFAULT_RETENTION, DEFAULT_STRING_MAX_LENGTH, STRING_MAX_LENGTH_CEILING,
};
pub use duration::parse_duration;
pub use path_match::{effective_match_glob, glob_from_target, pattern_from_target, PathPattern};
