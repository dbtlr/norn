//! `vault.describe` — describe this vault for an off-filesystem client.
//!
//! This is a NET-NEW read tool with no existing CLI command. It is the
//! placement aid for an agent that cannot `ls` the tree or read sibling
//! conventions: it returns three FORWARD facts so the agent can CONSTRUCT the
//! correct path for a new document itself (no reverse rule-inversion):
//!
//! 1. `folders` — the directories that currently hold documents (the folder
//!    tree), so the agent sees where each kind of doc lives.
//! 2. `path_rules` — the declared path → frontmatter-defaults mappings (i.e.
//!    "docs at `Workspaces/*/notes/*.md` get `type: note`"), so the agent knows
//!    which glob a new doc must satisfy to inherit the right defaults.
//! 3. `schema` — the configured frontmatter standards (field types, allowed
//!    values, required/forbidden fields), so the agent knows the field contract.
//!
//! The agent reads these, builds e.g. `Workspaces/norn/notes/my-note.md`, then
//! calls `vault.new`.
//!
//! **Source of truth.** Both the path rules and the schema live in the same
//! place: `ctx.config().validate.rules` (`Vec<ValidateRule>`). Each rule carries a
//! `match` selector (the path glob + frontmatter predicates), its
//! `frontmatter_defaults` (what `norn new` scaffolds), plus the schema fields
//! (`required_frontmatter`, `forbidden_frontmatter`, `field_types`,
//! `allowed_values`, `allowed_paths`). We surface these EXISTING structures
//! as-is rather than inventing a new rule/schema vocabulary:
//!
//! - `path_rules` is a focused projection — for every rule that declares a
//!   `match.path` glob, one `PathRule { glob, frontmatter_defaults }`. This is
//!   the minimal forward fact the placement story needs.
//! - `schema` is the full `validate` config serialized as `serde_json::Value`
//!   (the same shape the YAML declares), so no field-level standard is lost.
//!
//! **Output envelope:** unlike the other read tools, `DescribeOutput`'s fields
//! are `Vec<String>` + structs whose members are `String` / `serde_json::Value`,
//! so the struct derives `schemars::JsonSchema` directly. The root is an object,
//! satisfying rmcp 1.7.0's `outputSchema` constraint.
//!
//! **Core delegation.** All assembly logic lives in [`crate::describe::structure`]
//! and [`crate::describe::describe`], shared with the CLI `norn describe`
//! command so the two surfaces cannot drift. This module is now a thin
//! adapter: build a `Cache` + read `ctx.config`, delegate, return.
//!
//! **`data` + filter parity.** [`DescribeParams`] mirrors `CountParams`'s
//! filter-predicate surface (`eq`, `not_eq`, `in`, ... `unresolved_links`) plus
//! `data` / `by` / `limit`, exactly the CLI's `--data`/`--by`/`--limit` plus
//! the find-filter flags. `handle_with` builds a [`crate::filter_args::FilterArgs`]
//! from the params (mirroring `count.rs`'s `CountParams` → `FilterArgs`
//! conversion) and a `DataOptions` (mirroring `main.rs`'s CLI wiring: `by`
//! split on comma+trim, `want_data = data || !by.is_empty()`, `limit` default
//! 20), then calls the shared `crate::describe::describe` — the same call the
//! CLI makes — so the MCP `data` section is byte-for-byte what the CLI would
//! produce for the same filters.

use anyhow::Result;

use crate::describe::DescribeOutput;
use crate::filter_args::FilterArgs;
use crate::mcp::context::{RequestScope, VaultContext};

/// Parameters for `vault.describe`.
///
/// `data` (+ `by` / `limit`) opt into the contents-summary; the remaining
/// fields mirror `CountParams`'s filter-predicate surface verbatim (text, eq,
/// not_eq, in, not_in, starts_with, ends_with, contains, has, missing, before,
/// after, on, path, links_to, unresolved_links) so the MCP tool and `norn
/// describe --data` stay isomorphic on the filter surface.
#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct DescribeParams {
    /// Include the vault contents-summary (totals, field distributions, date bounds).
    #[serde(default)]
    pub data: bool,

    /// Explicit fields to distribute (comma-separated); bypasses identity-skip. Implies data.
    #[serde(default)]
    pub by: Option<String>,

    /// Max value-buckets per field (default 20; 0 = no cap).
    #[serde(default)]
    pub limit: Option<usize>,

    // ── Filter predicates (mirrors CountParams / FilterArgs) ────────────────
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

/// Project a [`DescribeParams`]'s filter predicates into a [`FilterArgs`].
/// Mirrors `count.rs`'s `CountParams` → `FilterArgs` construction field for
/// field, so the two tools cannot drift on filter semantics.
fn params_to_filter_args(params: &DescribeParams) -> FilterArgs {
    FilterArgs {
        text: params.text.clone(),
        eq: params.eq.clone(),
        not_eq: params.not_eq.clone(),
        r#in: params.r#in.clone(),
        not_in: params.not_in.clone(),
        starts_with: params.starts_with.clone(),
        ends_with: params.ends_with.clone(),
        contains: params.contains.clone(),
        has: params.has.clone(),
        missing: params.missing.clone(),
        before: params.before.clone(),
        after: params.after.clone(),
        on: params.on.clone(),
        path: params.path.clone(),
        links_to: params.links_to.clone(),
        unresolved_links: params.unresolved_links,
    }
}

/// Pure handler for `vault.describe`. Delegates to `handle_with`; kept as a
/// distinct entry point so the server registration and the tests both read
/// as calling "the" handler.
pub fn handle(
    ctx: &VaultContext,
    scope: &RequestScope,
    params: &DescribeParams,
) -> Result<DescribeOutput> {
    handle_with(ctx, scope, params)
}

/// Builds a `DataOptions` + `FilterArgs` from `params` and calls the shared
/// [`crate::describe::describe`] core — the same call the CLI makes for
/// `norn describe --data`, so the MCP `data` section cannot drift from it.
pub fn handle_with(
    ctx: &VaultContext,
    scope: &RequestScope,
    params: &DescribeParams,
) -> Result<DescribeOutput> {
    let cache = ctx.query_cache(scope)?;

    // Normalize `by` via the SHARED `normalize_by` helper — the same helper
    // the CLI arm uses — so the `want_data` gate below and the mode-selection
    // inside `summarize` agree across surfaces on a blank/whitespace-only
    // `--by` (NRN-103 F1 divergence: comma-split of `,` yields `["",""]`,
    // which must gate identically to the CLI's raw clap Vec).
    let split: Vec<String> = params
        .by
        .as_deref()
        .map(|s| s.split(',').map(str::to_string).collect())
        .unwrap_or_default();
    let by = crate::describe::data::normalize_by(&split);
    let want_data = params.data || !by.is_empty();
    let data = want_data.then(|| crate::describe::data::DataOptions {
        by,
        limit: params.limit.unwrap_or(20),
        ..Default::default()
    });

    let filters = params_to_filter_args(params);
    let config = scope.config();
    crate::describe::describe(&cache, &config, &filters, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::mcp::tools::scoped_shim! {
        fn handle(&DescribeParams) -> DescribeOutput;
        fn handle_with(&DescribeParams) -> DescribeOutput;
    }
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    /// Seed a vault with docs under `Workspaces/norn/notes/` and
    /// `Workspaces/norn/tasks/`, plus a `.norn/config.yaml` that declares:
    ///   - a path rule giving `Workspaces/*/notes/*.md` a `type: note` default,
    ///   - a path rule giving `Workspaces/*/tasks/*.md` a `type: task` default,
    ///   - a frontmatter schema (a `status` field with allowed_values).
    fn seeded_vault() -> (TempDir, Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-describe-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();

        let notes = root.join("Workspaces/norn/notes");
        let tasks = root.join("Workspaces/norn/tasks");
        std::fs::create_dir_all(&notes).unwrap();
        std::fs::create_dir_all(&tasks).unwrap();
        std::fs::write(
            notes.join("note1.md"),
            "---\ntype: note\ntitle: Note One\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            tasks.join("task1.md"),
            "---\ntype: task\nstatus: backlog\ntitle: Task One\n---\nbody\n",
        )
        .unwrap();

        let config_dir = root.join(".norn");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.yaml"),
            r#"validate:
  required_frontmatter:
    - title
  rules:
    - name: notes
      match:
        path: "Workspaces/{{workspace}}/notes/*.md"
      frontmatter_defaults:
        type: note
    - name: tasks
      match:
        path: "Workspaces/{{workspace}}/tasks/*.md"
      required_frontmatter:
        - status
      allowed_values:
        status:
          - backlog
          - in_progress
          - done
      frontmatter_defaults:
        type: task
"#,
        )
        .unwrap();

        (tmp, root)
    }

    #[test]
    fn handle_returns_folders_path_rules_and_schema() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let out = handle(&ctx, &DescribeParams::default()).expect("handle should succeed");

        // ── folders: the directory holding the notes doc is present ──────────
        assert!(
            out.folders.iter().any(|f| f == "Workspaces/norn/notes"),
            "folders should contain Workspaces/norn/notes, got: {:?}",
            out.folders
        );
        assert!(
            out.folders.iter().any(|f| f == "Workspaces/norn/tasks"),
            "folders should contain Workspaces/norn/tasks, got: {:?}",
            out.folders
        );
        // Folders are sorted + deduped.
        let mut sorted = out.folders.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted, out.folders, "folders must be sorted and deduped");

        // ── path_rules: the notes glob → type: note default is present ───────
        let notes_rule = out
            .path_rules
            .iter()
            .find(|r| r.glob == "Workspaces/{{workspace}}/notes/*.md")
            .unwrap_or_else(|| {
                panic!(
                    "path_rules must include the notes glob, got: {:?}",
                    out.path_rules.iter().map(|r| &r.glob).collect::<Vec<_>>()
                )
            });
        assert_eq!(
            notes_rule.frontmatter_defaults.get("type"),
            Some(&serde_json::json!("note")),
            "notes rule should give type: note, got: {:?}",
            notes_rule.frontmatter_defaults
        );

        // ── schema: the status allowed_values appear somewhere serialized ────
        let schema_str = serde_json::to_string(&out.schema).unwrap();
        assert!(
            schema_str.contains("allowed_values"),
            "schema should carry allowed_values, got: {schema_str}"
        );
        assert!(
            schema_str.contains("backlog")
                && schema_str.contains("in_progress")
                && schema_str.contains("done"),
            "schema should carry the status allowed_values, got: {schema_str}"
        );
        // required_frontmatter top-level + per-rule survive the round-trip.
        assert!(
            schema_str.contains("required_frontmatter") && schema_str.contains("title"),
            "schema should carry required_frontmatter title, got: {schema_str}"
        );
    }

    #[test]
    fn handle_on_unconfigured_vault_returns_empty_rules() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-describe-empty-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(root.join("top.md"), "---\ntype: note\n---\nbody\n").unwrap();

        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let out = handle(&ctx, &DescribeParams::default()).expect("handle should succeed");

        // No config → no path rules. The top-level doc folds to "".
        assert!(
            out.path_rules.is_empty(),
            "unconfigured vault has no path rules, got: {:?}",
            out.path_rules.iter().map(|r| &r.glob).collect::<Vec<_>>()
        );
        assert!(
            out.folders.iter().any(|f| f.is_empty()),
            "a top-level doc should contribute the empty-string folder, got: {:?}",
            out.folders
        );
        // No config → no creatable rules and no inbox.
        assert!(
            out.creatable_rules.is_empty(),
            "unconfigured vault has no creatable rules, got: {:?}",
            out.creatable_rules
                .iter()
                .map(|r| &r.name)
                .collect::<Vec<_>>()
        );
        assert!(
            out.inbox.is_none(),
            "unconfigured vault has no inbox, got: {:?}",
            out.inbox
        );
    }

    /// Seed a vault with a creatable rule (has `name` + `target`) plus a non-creatable
    /// path rule (has `match.path` but no `target`). Also configure an inbox.
    fn vault_with_creatable_rule() -> (TempDir, Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-describe-creatable-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();

        // Create a doc so collect_folders has something to work with.
        let tasks_dir = root.join("Workspaces/norn/tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("task1.md"),
            "---\ntype: task\ntitle: Task One\n---\nbody\n",
        )
        .unwrap();

        let config_dir = root.join(".norn");
        std::fs::create_dir_all(&config_dir).unwrap();
        // Two rules: `task` uses `target` (creatable, no match.path), `notes`
        // uses `match.path` (path rule only). To avoid the conflict-defaults
        // guard (which can't prove disjointness when one rule uses `target`),
        // the `notes` rule only sets `source: notes` (a non-conflicting field).
        std::fs::write(
            config_dir.join("config.yaml"),
            "inbox:\n  path: Inbox\nvalidate:\n  rules:\n    - name: task\n      target: \"Workspaces/{{var.workspace}}/tasks/{{title|slugify}}.md\"\n      body: \"## Context\\n\"\n      frontmatter_defaults:\n        type: task\n    - name: notes\n      match:\n        path: \"Workspaces/norn/notes/*.md\"\n      frontmatter_defaults:\n        source: notes\n",
        )
        .unwrap();

        (tmp, root)
    }

    #[test]
    fn describe_surfaces_creatable_rule_and_inbox() {
        let (_tmp, root) = vault_with_creatable_rule();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let out = handle(&ctx, &DescribeParams::default()).expect("handle should succeed");

        // ── creatable_rules: the "task" rule with target is present ──────────
        assert_eq!(
            out.creatable_rules.len(),
            1,
            "expected one creatable rule (task), got: {:?}",
            out.creatable_rules
                .iter()
                .map(|r| &r.name)
                .collect::<Vec<_>>()
        );
        let task_rule = &out.creatable_rules[0];
        assert_eq!(task_rule.name, "task");
        assert_eq!(
            task_rule.target,
            "Workspaces/{{var.workspace}}/tasks/{{title|slugify}}.md"
        );
        // required_vars: "workspace" extracted from {{var.workspace}}.
        assert_eq!(
            task_rule.required_vars,
            vec!["workspace".to_string()],
            "required_vars should be [\"workspace\"], got: {:?}",
            task_rule.required_vars
        );
        // frontmatter_defaults carries type: task.
        assert_eq!(
            task_rule.frontmatter_defaults.get("type"),
            Some(&serde_json::json!("task")),
            "creatable rule should give type: task, got: {:?}",
            task_rule.frontmatter_defaults
        );
        // body scaffold is present.
        assert!(
            task_rule.body.is_some(),
            "creatable rule should carry a body scaffold"
        );
        assert!(
            task_rule.body.as_deref().unwrap().contains("Context"),
            "body scaffold should contain 'Context', got: {:?}",
            task_rule.body
        );

        // ── inbox: present and set to "Inbox" ────────────────────────────────
        assert_eq!(
            out.inbox.as_deref(),
            Some("Inbox"),
            "inbox should be Some(\"Inbox\"), got: {:?}",
            out.inbox
        );

        // ── path_rules: the non-creatable notes rule is in path_rules ────────
        // (the task rule has target, not match.path, so it does NOT appear in path_rules)
        assert!(
            out.path_rules
                .iter()
                .any(|r| r.glob == "Workspaces/norn/notes/*.md"),
            "path_rules should include the notes glob, got: {:?}",
            out.path_rules.iter().map(|r| &r.glob).collect::<Vec<_>>()
        );
        assert!(
            !out.path_rules
                .iter()
                .any(|r| r.name.as_deref() == Some("task")),
            "task (creatable) rule must NOT appear in path_rules"
        );
    }

    #[test]
    fn describe_data_matches_cli_summarize() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        // MCP path with data: true
        let params = DescribeParams {
            data: true,
            ..Default::default()
        };
        let out = handle_with(&ctx, &params).expect("handle_with");
        let data = out.data.expect("data present");

        // Direct core path
        let cache = ctx.query_cache_unscoped().unwrap();
        let docs = cache
            .documents_matching(
                &crate::filter_args::build_document_query(
                    &crate::filter_args::FilterArgs::default(),
                )
                .unwrap(),
            )
            .unwrap();
        let expected = crate::describe::data::summarize(
            &docs,
            &ctx.config(),
            &crate::describe::data::DataOptions::default(),
        );

        assert_eq!(data.total, expected.total);
        assert_eq!(data.fields, expected.fields);
        assert_eq!(data.skipped, expected.skipped);
        assert_eq!(data.dates, expected.dates);
    }

    /// A non-default filter (`eq type:note`) passed through `handle_with` must
    /// scope the summary identically to a direct `summarize` call over
    /// `documents_matching` with the SAME filter — exercising the 16-field
    /// `params_to_filter_args` mapping end to end (mirrors `count.rs`'s
    /// `handle_eq_filter_narrows_count`).
    #[test]
    fn describe_data_respects_filter() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let params = DescribeParams {
            data: true,
            eq: vec!["type:note".into()],
            ..Default::default()
        };
        let out = handle_with(&ctx, &params).expect("handle_with");
        let data = out.data.expect("data present");

        // Direct core path with the SAME eq filter.
        let cache = ctx.query_cache_unscoped().unwrap();
        let filters = crate::filter_args::FilterArgs {
            eq: vec!["type:note".into()],
            ..Default::default()
        };
        let docs = cache
            .documents_matching(&crate::filter_args::build_document_query(&filters).unwrap())
            .unwrap();
        let expected = crate::describe::data::summarize(
            &docs,
            &ctx.config(),
            &crate::describe::data::DataOptions::default(),
        );

        assert_eq!(
            data.total, 1,
            "seeded_vault has exactly one type:note doc, got total={}",
            data.total
        );
        assert_eq!(data.total, expected.total);
        assert_eq!(data.fields, expected.fields);
    }

    #[test]
    fn describe_without_data_flag_omits_summary() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let out = handle_with(&ctx, &DescribeParams::default()).expect("handle");
        assert!(out.data.is_none(), "no data flag ⇒ no summary section");
    }
}
