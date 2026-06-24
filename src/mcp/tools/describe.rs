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
//! place: `ctx.config.validate.rules` (`Vec<ValidateRule>`). Each rule carries a
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

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cli::{FindArgs, SortPaginateArgs};
use crate::filter_args::FilterArgs;
use crate::mcp::context::VaultContext;

/// Parameters for `vault.describe`. Empty — describe takes no args in v1.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct DescribeParams {}

/// A single declared path rule: which glob gets which frontmatter defaults.
///
/// Surfaced from a `ValidateRule` that declares a `match.path`. The
/// `frontmatter_defaults` map is the rule's `frontmatter_defaults` verbatim
/// (e.g. `{"type": "note"}`) — the values `norn new` scaffolds onto a doc
/// created at a path matching `glob`. An agent reads `glob` to learn where a
/// kind of doc lives, and `frontmatter_defaults` to learn what it inherits.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct PathRule {
    /// The rule's `match.path` glob (e.g. `Workspaces/{{workspace}}/notes/*.md`).
    pub glob: String,
    /// Optional rule name, if the config declared one.
    pub name: Option<String>,
    /// The frontmatter defaults a doc at a matching path inherits, as declared.
    /// Empty object when the rule sets no defaults (it is still a placement
    /// signal — the glob tells the agent where that kind of doc lives).
    pub frontmatter_defaults: serde_json::Value,
}

/// A rule that can be used to create a new document via `vault.new { rule: "..." }`.
///
/// Only rules that declare BOTH a `name` AND a `target` template are creatable.
/// An agent can call `vault.new { rule: name, title: "...", vars: {...} }` to
/// create a document at the path derived from `target`, without knowing the
/// concrete path in advance.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CreatableRule {
    /// The rule's declared name — pass this as `vault.new { rule: name }`.
    pub name: String,
    /// The path template (e.g. `Workspaces/{{var.workspace}}/tasks/{{title|slugify}}.md`).
    /// Shows the agent what the concrete path will look like once rendered.
    pub target: String,
    /// Variable names (from `{{var.X}}` / `{{path.X}}` tokens) the template requires.
    /// The agent must supply these via `vault.new { vars: { "workspace": "..." } }`.
    pub required_vars: Vec<String>,
    /// The frontmatter defaults a doc created with this rule inherits.
    pub frontmatter_defaults: serde_json::Value,
    /// Optional body scaffold template for the new document's body.
    /// Rendered with the same substitution context used for path generation.
    pub body: Option<String>,
}

/// Structured output for `vault.describe`. Root is `type: object`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DescribeOutput {
    /// Distinct vault-relative directories that currently hold documents,
    /// sorted. The vault root is represented as `""` (docs at top level). This
    /// is the folder tree the agent uses to see where each kind of doc lives.
    pub folders: Vec<String>,
    /// Declared path rules: for each config rule with a `match.path`, the glob
    /// plus the frontmatter defaults a doc at a matching path inherits.
    pub path_rules: Vec<PathRule>,
    /// Rules that can be used with `vault.new { rule: "name" }` to create a
    /// document at a derived path. Each entry carries the rule name, its target
    /// template, the required variable names, frontmatter defaults, and optional
    /// body scaffold. Separate from `path_rules` so the existing contract is
    /// undisturbed — an agent that already parses `path_rules` keeps working.
    pub creatable_rules: Vec<CreatableRule>,
    /// Configured inbox path (from `inbox.path` in the vault config), if any.
    /// When present, `vault.new { title: "..." }` (no path, no rule) routes the
    /// new document to `<inbox>/<title|slugify>.md`.
    pub inbox: Option<String>,
    /// The full configured frontmatter schema/standards — the `validate` config
    /// serialized verbatim (required/forbidden fields, field types, allowed
    /// values, per-rule selectors). `serde_json::Value` so no standard is lost.
    pub schema: serde_json::Value,
}

/// Pure handler for `vault.describe`.
///
/// Assembles the three forward facts:
/// - `folders` from an unbounded paths query folded to distinct parent dirs.
///   Reuses the shared `find::query` seam (default `{path, frontmatter}`
///   projection) with `no_limit` so the full document set is folded — matching
///   the find/get/count behavior of including `files.ignore`-d docs (Task 6).
/// - `path_rules` + `schema` from the warm `ctx.config.validate` config.
pub fn handle(ctx: &VaultContext) -> Result<DescribeOutput> {
    let folders = collect_folders(ctx)?;

    let path_rules = ctx
        .config
        .validate
        .rules
        .iter()
        .filter_map(|rule| {
            rule.r#match.path.as_ref().map(|glob| PathRule {
                glob: glob.clone(),
                name: rule.name.clone(),
                frontmatter_defaults: serde_json::to_value(&rule.frontmatter_defaults)
                    .unwrap_or(serde_json::Value::Null),
            })
        })
        .collect();

    // Creatable rules: rules that have BOTH a `name` AND a `target` template.
    // These are the rules an agent can pass to `vault.new { rule: "..." }`.
    // We use a separate `creatable_rules` field rather than extending `PathRule`
    // to keep the existing `path_rules` contract undisturbed — agents that already
    // parse `path_rules` continue to work unchanged.
    let creatable_rules = ctx
        .config
        .validate
        .rules
        .iter()
        .filter_map(|rule| {
            // A rule is creatable iff it has both `name` and `target`.
            let name = rule.name.as_ref()?;
            let target = rule.target.as_ref()?;
            let required_vars = crate::new::generate::referenced_vars(target);
            Some(CreatableRule {
                name: name.clone(),
                target: target.clone(),
                required_vars,
                frontmatter_defaults: serde_json::to_value(&rule.frontmatter_defaults)
                    .unwrap_or(serde_json::Value::Null),
                body: rule.body.clone(),
            })
        })
        .collect();

    let inbox = ctx.config.vault_config.inbox.path.clone();

    let schema = serde_json::to_value(&ctx.config.validate)?;

    Ok(DescribeOutput {
        folders,
        path_rules,
        creatable_rules,
        inbox,
        schema,
    })
}

/// Query every document's path (no limit) and fold to the sorted, deduped set of
/// distinct parent directories. Vault-root docs contribute `""`.
fn collect_folders(ctx: &VaultContext) -> Result<Vec<String>> {
    let cache = ctx.query_cache()?;

    // Empty filter, no limit → every document. Default `{path, frontmatter}`
    // projection gives us each doc's vault-relative `path`.
    let args = FindArgs {
        filters: FilterArgs::default(),
        all: true,
        paging: SortPaginateArgs {
            sort: None,
            desc: false,
            limit: None,
            no_limit: true,
            starts_at: 1,
        },
        format: None,
        all_cols: false,
        col: Vec::new(),
        no_pager: false,
    };

    let documents = crate::find::query::query(&cache, &args, None)?;

    let mut folders: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for doc in &documents {
        let Some(path) = doc.get("path").and_then(|p| p.as_str()) else {
            continue;
        };
        // Parent directory of the vault-relative path. Top-level docs → "".
        let parent = match path.rfind('/') {
            Some(idx) => &path[..idx],
            None => "",
        };
        folders.insert(parent.to_string());
    }

    Ok(folders.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
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

        let out = handle(&ctx).expect("handle should succeed");

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
        let out = handle(&ctx).expect("handle should succeed");

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

        let out = handle(&ctx).expect("handle should succeed");

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
}
