//! Single source of truth for the directory names, path globs, and enumerated
//! values that the config emitter (`crate::config`) and the document emitters
//! (`crate::zoo`, `crate::expansion`) must agree on. Change a value here and
//! both the emitted `.norn/config.yaml` rules and the documents those rules
//! judge move together — no drift between the schema and the fixtures.
//!
//! Deliberately plain `pub const` items: the dir and its allowed-paths glob are
//! stored as an explicit *pair*, not derived one from the other. No
//! glob-deriving machinery — the redundancy is the point (a glob can diverge
//! from `<dir>/**/*.md` intentionally, e.g. the log carve-outs).

/// Allowed `status` values for `task`/`phase` documents. Consumed by the
/// config's `task-rule.allowed_values` and by the expansion emitter when it
/// picks a valid status for a generated task or phase.
pub const STATUS_VALUES: &[&str] = &["backlog", "active", "done"];

/// Directory that valid `task` documents live under.
pub const TASKS_DIR: &str = "tasks";
/// Directory that valid `phase` documents live under.
pub const PHASES_DIR: &str = "phases";
/// Directory the `no-legacy` rule governs.
pub const NOTES_DIR: &str = "notes";
/// Directory the `dated-log` rule governs.
pub const LOGS_DIR: &str = "logs";
/// Directory exempted from validation via `validate.ignore`.
pub const TEMPLATES_DIR: &str = "templates";
/// Directory whose `{a,b}` alternation is exempted via `validate.ignore`.
pub const DRAFTS_DIR: &str = "drafts";
/// Directory dropped from the graph entirely via `files.ignore`.
pub const IGNORED_DIR: &str = "ignored";

/// `allowed_paths` glob for `task` documents (paired with [`TASKS_DIR`]).
pub const TASKS_GLOB: &str = "tasks/**/*.md";
/// `allowed_paths` glob for `phase` documents (paired with [`PHASES_DIR`]).
pub const PHASES_GLOB: &str = "phases/**/*.md";

/// Sentinel file marking a directory as generator-owned. Hidden (dot-prefixed)
/// so it stays out of norn's graph.
pub const SENTINEL_FILE: &str = ".norn-fixture-vault";
/// Exact sentinel contents. The bin verifies these bytes (and that the
/// sentinel is a regular, non-symlink file) before clearing a directory —
/// a name-only check would let any stray file named like the sentinel
/// authorize a recursive delete.
pub const SENTINEL_CONTENT: &str = "norn-fixtures generated vault — safe to delete\n";
