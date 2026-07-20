//! Mutation-verb execute seams (set/new). Each builds — and, when confirmed,
//! applies — a MigrationPlan against the warm cache, returning a wire Report.
mod coerce;
pub mod new;
pub mod set;

use camino::Utf8PathBuf;

/// What the owner drives: the verb's wire Report plus the paths a CONFIRMED
/// write touched (empty for a forecast or a refusal) — the owner commits the
/// cache increment for exactly these.
pub struct MutationExecution<R> {
    pub report: R,
    pub touched_paths: Vec<Utf8PathBuf>,
}

/// Warn-class wikilink resolution: does `value`'s stem resolve to a unique doc?
/// Empty → unresolved; >1 → ambiguous; exactly one → no warning. Shared by both
/// mutation seams.
pub(crate) fn wikilink_warnings(
    index: &crate::domain::GraphIndex,
    field: &str,
    value: &str,
) -> Vec<norn_wire::MutationWarning> {
    let target = value
        .strip_prefix("[[")
        .and_then(|s| s.strip_suffix("]]"))
        .unwrap_or(value);
    let canonical = target
        .split('#')
        .next()
        .unwrap_or(target)
        .split('|')
        .next()
        .unwrap_or(target)
        .to_lowercase();
    let matches = index
        .documents
        .iter()
        .filter(|d| d.stem.to_lowercase() == canonical)
        .count();
    match matches {
        0 => vec![norn_wire::MutationWarning {
            code: "wikilink-unresolved".into(),
            message: format!("unresolved wikilink in {field}: [[{target}]]"),
        }],
        1 => Vec::new(),
        _ => vec![norn_wire::MutationWarning {
            code: "wikilink-ambiguous".into(),
            message: format!("ambiguous wikilink in {field}: [[{target}]]"),
        }],
    }
}

/// The owner index options derived from a vault config's ignore + alias-field —
/// the second-scan policy `apply_migration_plan` uses for the owner-set barrier.
/// A verb with no logical preconditions never pays for that scan, but the value
/// is cheap to build and keeps the two verbs identical.
pub(crate) fn owner_index_options(
    config: Option<&crate::standards::VaultConfig>,
) -> crate::graph::IndexOptions {
    match config {
        Some(cfg) => crate::graph::IndexOptions {
            ignore: cfg.files.ignore.clone(),
            alias_field: cfg.links.alias_field.clone(),
        },
        None => crate::graph::IndexOptions::default(),
    }
}
