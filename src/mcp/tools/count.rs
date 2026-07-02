//! `vault.count` — total or grouped document counts.
//!
//! The pure handler reuses [`crate::count::run`], the exact code path behind
//! `norn count`. It returns the same [`CountOutput`] enum the CLI renders, so the
//! MCP surface and the CLI can never drift on filter evaluation or grouping logic.
//!
//! **Output envelope:** rmcp 1.7.0 requires a tool's `outputSchema` root to be
//! `type: object`. `CountOutput` is `#[serde(untagged)]`, which produces a root
//! that varies by variant (object with `total` XOR object with `by`/`total`/`groups`)
//! — a union, not a single typed object, so the schema cannot be an object at the
//! root. We therefore project into a flat `CountEnvelope` struct that covers both
//! variants in a single object: `total` is always present; `by` and `groups` are
//! `Option`-al and set only when a `--by` field was requested.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cli::{CountArgs, CountFormat};
use crate::count::CountOutput;
use crate::filter_args::FilterArgs;
use crate::mcp::context::VaultContext;

/// Parameters for `vault.count`.
///
/// Mirrors the agent-useful slice of `norn count`'s flags: the full find-filter
/// surface (text, eq, not_eq, in, not_in, starts_with, ends_with, contains, has,
/// missing, before, after, on, path, links_to, unresolved_links) plus `by` for
/// grouping. `--format` is omitted from v1 — the MCP tool always returns the
/// structured envelope.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct CountParams {
    /// Frontmatter field(s) to group counts by — comma-separated, exactly the
    /// CLI's `--by` token (e.g. `"project,lifecycle"`). Without `by`, only
    /// the total is returned. One field returns a string `by` and a flat
    /// value→count `groups` map; several fields return an array `by` and
    /// nested `groups` (one map level per field, counts at the leaves).
    #[serde(default)]
    pub by: Option<String>,

    // ── Filter predicates (mirrors FilterArgs) ──────────────────────────────
    /// Full-text body substring. Case-insensitive.
    #[serde(default)]
    pub text: Option<String>,

    /// Frontmatter equality predicates `field:value`. Repeatable; all must match.
    #[serde(default)]
    pub eq: Vec<String>,

    /// Frontmatter inequality predicates `field:value`. Repeatable.
    #[serde(default)]
    pub not_eq: Vec<String>,

    /// Frontmatter ANY-of predicates `field:V1,V2,...`. Repeatable.
    #[serde(default)]
    #[serde(rename = "in")]
    pub r#in: Vec<String>,

    /// Frontmatter NOT-in predicates `field:V1,V2,...`. Repeatable.
    #[serde(default)]
    pub not_in: Vec<String>,

    /// Frontmatter prefix predicates `field:VALUE` — the field (or any array
    /// element) starts with VALUE. Case-sensitive. Repeatable; all must match.
    #[serde(default)]
    pub starts_with: Vec<String>,

    /// Frontmatter suffix predicates `field:VALUE` — the field (or any array
    /// element) ends with VALUE. Case-sensitive. Repeatable.
    #[serde(default)]
    pub ends_with: Vec<String>,

    /// Frontmatter substring predicates `field:VALUE` — the field (or any
    /// array element) contains VALUE. Case-sensitive. Repeatable.
    #[serde(default)]
    pub contains: Vec<String>,

    /// Frontmatter fields that must be present (non-null). Repeatable.
    #[serde(default)]
    pub has: Vec<String>,

    /// Frontmatter fields that must be absent or null. Repeatable.
    #[serde(default)]
    pub missing: Vec<String>,

    /// Date-before predicates `field:DATE`. ISO 8601. Repeatable.
    #[serde(default)]
    pub before: Vec<String>,

    /// Date-after predicates `field:DATE`. ISO 8601. Repeatable.
    #[serde(default)]
    pub after: Vec<String>,

    /// Date-on predicates `field:DATE`. Accepts `today`. Repeatable.
    #[serde(default)]
    pub on: Vec<String>,

    /// Path glob patterns. Repeatable.
    #[serde(default)]
    pub path: Vec<String>,

    /// Documents whose outgoing links resolve to TARGET. Repeatable; AND'd.
    #[serde(default)]
    pub links_to: Vec<String>,

    /// Include only documents with at least one unresolved link.
    #[serde(default)]
    pub unresolved_links: bool,
}

/// Flat output envelope for `vault.count`.
///
/// Covers every variant of [`CountOutput`] in a single `type: object` root so
/// rmcp's schema validation passes at server startup, mirroring the CLI's
/// `--format json` payloads exactly:
/// - `total` — always present; the number of matching documents.
/// - `by` — absent without grouping; the field name (string) for one key;
///   the key list (array) for several.
/// - `groups` — absent without grouping; a flat value→count map for one key;
///   nested maps (one level per key, counts at the leaves) for several.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CountEnvelope {
    /// Total number of matching documents.
    pub total: usize,
    /// Grouping key(s): a string for single-key grouping, an array of
    /// strings for multi-key (set when `by` was requested).
    pub by: Option<serde_json::Value>,
    /// Per-value document counts, sorted by field value: flat for one key,
    /// nested for several (set when `by` was requested).
    pub groups: Option<serde_json::Value>,
}

impl CountEnvelope {
    fn from_output(output: CountOutput) -> Self {
        match output {
            CountOutput::Total { total } => Self {
                total,
                by: None,
                groups: None,
            },
            CountOutput::Grouped { by, total, groups } => Self {
                total,
                by: Some(serde_json::Value::String(by)),
                groups: Some(serde_json::to_value(groups).expect("count groups serialize")),
            },
            CountOutput::GroupedMulti { by, total, groups } => Self {
                total,
                by: Some(serde_json::to_value(by).expect("count by serialize")),
                groups: Some(serde_json::to_value(groups).expect("count groups serialize")),
            },
        }
    }
}

/// Pure handler for `vault.count`. Opens a fresh query cache (per-call freshness),
/// constructs [`CountArgs`] with `norn count`'s defaults, and runs the count path.
pub fn handle(ctx: &VaultContext, p: CountParams) -> Result<CountEnvelope> {
    let cache = ctx.query_cache()?;

    let args = CountArgs {
        // Same comma token as the CLI flag; empty segments are rejected by
        // `count::run`'s empty-field guard, matching CLI behavior.
        by: p
            .by
            .as_deref()
            .map(|token| token.split(',').map(str::to_string).collect())
            .unwrap_or_default(),
        filters: FilterArgs {
            text: p.text,
            eq: p.eq,
            not_eq: p.not_eq,
            r#in: p.r#in,
            not_in: p.not_in,
            starts_with: p.starts_with,
            ends_with: p.ends_with,
            contains: p.contains,
            has: p.has,
            missing: p.missing,
            before: p.before,
            after: p.after,
            on: p.on,
            path: p.path,
            links_to: p.links_to,
            unresolved_links: p.unresolved_links,
        },
        // `--format` is CLI-only; the MCP tool always returns the structured
        // envelope, so we pass the default (Text) which count::run ignores.
        format: CountFormat::Text,
    };

    let output = crate::count::run(&cache, &args)?;
    Ok(CountEnvelope::from_output(output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    /// Seed a temp vault with 3 docs: 2 `type: note`, 1 `type: task`.
    fn seeded_vault() -> (TempDir, Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-count-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(
            root.join("note1.md"),
            "---\ntype: note\ntitle: Note One\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            root.join("note2.md"),
            "---\ntype: note\ntitle: Note Two\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            root.join("task1.md"),
            "---\ntype: task\ntitle: Task One\n---\nbody\n",
        )
        .unwrap();
        (tmp, root)
    }

    /// (a) No filter, no `by` → total == 3.
    #[test]
    fn handle_no_filter_returns_total_three() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let envelope = handle(&ctx, CountParams::default()).expect("handle should succeed");

        assert_eq!(
            envelope.total, 3,
            "expected total 3 for 3 seeded docs, got {}",
            envelope.total
        );
        assert!(
            envelope.by.is_none(),
            "expected no `by` field in total mode"
        );
        assert!(
            envelope.groups.is_none(),
            "expected no `groups` in total mode"
        );
    }

    /// (b) Grouped by `type` → groups: {note: 2, task: 1}, total: 3.
    #[test]
    fn handle_grouped_by_type_returns_correct_counts() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let envelope = handle(
            &ctx,
            CountParams {
                by: Some("type".into()),
                ..CountParams::default()
            },
        )
        .expect("handle should succeed");

        assert_eq!(
            envelope.total, 3,
            "grouped total should still be 3, got {}",
            envelope.total
        );
        assert_eq!(
            envelope.by,
            Some(serde_json::json!("type")),
            "single-key `by` must stay a plain string"
        );

        let groups = envelope
            .groups
            .as_ref()
            .expect("groups must be present in grouped mode");
        assert_eq!(
            groups["note"].as_u64(),
            Some(2),
            "note group should have count 2, got {groups:?}"
        );
        assert_eq!(
            groups["task"].as_u64(),
            Some(1),
            "task group should have count 1, got {groups:?}"
        );
        assert_eq!(
            groups.as_object().unwrap().len(),
            2,
            "expected exactly 2 groups (note, task), got {groups:?}"
        );
    }

    /// Multi-key `by` (comma token) nests groups and returns an array `by`.
    #[test]
    fn handle_multi_key_by_nests_groups() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let envelope = handle(
            &ctx,
            CountParams {
                by: Some("type,title".into()),
                ..CountParams::default()
            },
        )
        .expect("handle should succeed");

        assert_eq!(
            envelope.by,
            Some(serde_json::json!(["type", "title"])),
            "multi-key `by` must be the key array"
        );
        let groups = envelope.groups.as_ref().expect("groups present");
        assert_eq!(groups["note"]["Note One"].as_u64(), Some(1), "{groups:?}");
        assert_eq!(groups["note"]["Note Two"].as_u64(), Some(1), "{groups:?}");
        assert_eq!(groups["task"]["Task One"].as_u64(), Some(1), "{groups:?}");
    }

    /// An empty segment in the comma token is rejected, matching the CLI.
    #[test]
    fn handle_empty_by_segment_errors() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let err = handle(
            &ctx,
            CountParams {
                by: Some("type,".into()),
                ..CountParams::default()
            },
        )
        .expect_err("empty --by segment should error");
        assert!(err.to_string().contains("empty field name"), "{err}");
    }

    /// Filter with `eq` reduces the counted set.
    #[test]
    fn handle_eq_filter_narrows_count() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let envelope = handle(
            &ctx,
            CountParams {
                eq: vec!["type:note".into()],
                ..CountParams::default()
            },
        )
        .expect("handle should succeed");

        assert_eq!(
            envelope.total, 2,
            "eq filter for type:note should yield 2, got {}",
            envelope.total
        );
    }
}
