//! Hand-authored canned `--help` examples per command path.
//!
//! Per the Phase 2 design spec, examples are biased toward fewer — empty is
//! the correct answer for many commands. The match below returns `vec![]`
//! for any command path with no authored examples; the renderer skips the
//! `EXAMPLES` section when the table is empty.
//!
//! Each entry is `(command_line, comment)`. Comments are ≤60 chars, lowercase
//! except for required literals, no trailing period. The command line uses
//! the literal `vault` prefix; the renderer styles tokens per palette.

use crate::help::model::LiveExample;

/// Return canned examples for the given command path string (e.g. `"vault find"`).
///
/// Returns `vec![]` for unknown paths and for paths intentionally without
/// examples. The renderer's `EXAMPLES` section is suppressed when the table
/// is empty.
pub fn examples_for(cmd_path: &str) -> Vec<(String, String)> {
    let pairs: &[(&str, &str)] = match cmd_path {
        "vault" => &[
            (
                "vault find --eq type:note --limit 5",
                "5 notes by default sort",
            ),
            (
                "vault validate --format json",
                "machine-readable validation findings",
            ),
            (
                "vault repair plan --out plan.json",
                "generate a frontmatter repair plan",
            ),
        ],
        "vault find" => &[
            (
                "vault find --eq type:note --limit 5",
                "5 notes; default sort",
            ),
            (
                "vault find --text reorg --format paths",
                "full-text search; pipe-friendly paths",
            ),
            (
                "vault find --has aliases --col title,aliases",
                "docs that declare aliases",
            ),
            (
                "vault find --in type:note,log --sort modified --desc",
                "two types, newest first",
            ),
        ],
        "vault count" => &[
            ("vault count", "total document count in the vault"),
            (
                "vault count --eq type:note --by status",
                "notes only, grouped by status",
            ),
            (
                "vault count --path 'Workspaces/**/tasks/*.md' --by status",
                "one project's tasks, grouped by status",
            ),
        ],
        "vault show" => &[
            ("vault show foo", "show one doc by case-insensitive stem"),
            (
                "vault show '[[foo]]'",
                "wikilink input; anchor/alias suffixes stripped",
            ),
            (
                "vault show foo --col incoming_links",
                "backlinks only (the absorbed `links backlinks` job)",
            ),
            (
                "vault show a.md b.md c.md",
                "multiple targets, one record per doc",
            ),
        ],
        "vault validate" => &[
            (
                "vault validate",
                "human-readable findings on the configured vault",
            ),
            (
                "vault validate --format json",
                "machine-readable findings for pipelines",
            ),
            (
                "vault validate --severity error",
                "errors only; skip warnings",
            ),
            (
                "vault validate --code link-unresolved,link-ambiguous",
                "broken + ambiguous links (replaces `vault links unresolved`)",
            ),
            (
                "vault validate --code link-unresolved,link-ambiguous --format paths",
                "unique source paths only; pipe-friendly",
            ),
        ],
        "vault repair plan" => &[
            (
                "vault repair plan --out plan.json",
                "write a frontmatter repair plan",
            ),
            (
                "vault repair plan --format json",
                "preview the plan on stdout",
            ),
            (
                "vault repair plan --severity error",
                "plan only error-level findings",
            ),
        ],
        "vault repair apply" => &[
            (
                "vault repair apply plan.json --dry-run",
                "preview changes without writing",
            ),
            (
                "vault repair apply plan.json",
                "apply a previously-generated plan",
            ),
            (
                "vault repair apply plan.json --verify",
                "apply then re-validate",
            ),
        ],
        // ── Default tier: 1-2 examples each ─────────────────────────────────
        "vault init" => &[(
            "vault init",
            "scaffold .vault/config.yaml in the current directory",
        )],
        "vault config show" => &[
            ("vault config show", "effective config: paths + counts"),
            (
                "vault config show --format json",
                "machine-readable config for pipelines",
            ),
        ],
        "vault cache rebuild" => &[(
            "vault cache rebuild",
            "delete and rebuild the cache from scratch",
        )],
        "vault cache status" => &[(
            "vault cache status",
            "cache path, size, doc and link counts",
        )],
        "vault cache index" => &[
            (
                "vault cache index",
                "incremental refresh via mtime+size check",
            ),
            (
                "vault cache index --force-hash",
                "hash every file; bypass cheap-check",
            ),
        ],
        "vault files" => &[
            ("vault files", "every inventoried file under the vault"),
            ("vault files --format paths", "just paths; pipe-friendly"),
        ],
        "vault repair links" => &[
            (
                "vault repair links",
                "report link and path repair risks; no writes",
            ),
            (
                "vault repair links --target old.md --move-to new.md",
                "preview link risk if target were moved",
            ),
        ],

        // ── Thin tier: 0-1 examples each ────────────────────────────────────
        "vault completions init" => &[(
            "vault completions init zsh",
            "emit zsh completion script to stdout",
        )],
        "vault completions install" => &[(
            "vault completions install",
            "install for the shell detected from $SHELL",
        )],

        // Thin commands without arms (intentionally empty — flag block self-explains):
        // vault cache clear, vault config validate,
        // vault config migrate, vault config edit
        _ => &[],
    };
    pairs
        .iter()
        .map(|(cmd, comment)| (cmd.to_string(), comment.to_string()))
        .collect()
}

/// Return Phase 4 conceptual sections for the given command path.
///
/// Each entry is `(heading, body)`. Headings render in `dim` bold uppercase
/// (the renderer uppercases them); bodies are markdown-light paragraphs
/// separated by blank lines. Returns `vec![]` for command paths that have
/// no conceptual sections — the renderer's block is suppressed in that case.
///
/// Sections are emitted on `--help` only, after EXAMPLES / LIVE EXAMPLES
/// and before GLOBAL OPTIONS. Headings render in `dim` bold uppercase; body
/// paragraphs split on blank lines. Numbered lists and JSON blocks within a
/// paragraph keep their internal indentation.
pub fn conceptual_sections_for(cmd_path: &str) -> Vec<(String, String)> {
    let pairs: &[(&str, &str)] = match cmd_path {
        "vault validate" => &[
            (
                "How validation works",
                "Validate reads `.vault/config.yaml` for the rules that shape your vault: required frontmatter fields, allowed values, expected types, and path scoping. Each rule produces findings with a stable code and a severity (`error`, `warning`, `info`).\n\nFindings cover three surfaces. Frontmatter findings come from schema rules — codes like `frontmatter-required-field-missing` and `frontmatter-disallowed-value`. Link findings come from graph facts — `link-unresolved` and `link-ambiguous`. Document diagnostics come from parse — malformed frontmatter, encoding issues. Validate never writes files.\n\nExit code is `1` when any finding has severity `error`, `0` otherwise. Pipelines gate on this exit code.\n\nTriage filters combine with AND across types and OR within a type. `--severity error --code frontmatter-required-field-missing` returns errors that match that code. `--code link-unresolved --code link-ambiguous` returns either. `--path 'notes/**'` scopes to a path glob; `--field`, `--rule`, `--target`, and `--reason` narrow further.",
            ),
            (
                "Finding codes",
                "Codes identify validation findings. Filter with --code <code>.\n\nlink-unresolved             A link target, anchor, or block-ref is missing.\nlink-ambiguous              A wikilink resolves to multiple candidates.\nfrontmatter-required-field-missing\n                            A required frontmatter field is absent.\nfrontmatter-disallowed-value\n                            A field's value is not in the configured set.\nfrontmatter-invalid-type    A field's value doesn't match its declared type.\nfrontmatter-forbidden-field A field that the rule forbids is present.\nfrontmatter-alias-shadowed-by-stem\n                            An alias matches another doc's stem; the alias is dead because stem resolution wins.\nfrontmatter-alias-duplicate-across-docs\n                            Two or more docs claim the same alias; wikilinks resolving via that alias will be ambiguous.\nfrontmatter-alias-malformed The alias field contains a non-scalar value.\ndocument-misrouted          A doc is in a directory the rule's path selector excludes.",
            ),
        ],
        "vault repair plan" => &[(
            "The plan/apply boundary",
            "Repair runs in two halves. Plan reads validate findings and emits a JSON artifact describing every change it would make. Plan never writes to vault documents. Apply consumes that artifact and writes the changes; preconditions are checked before any file is touched.\n\nPlan classifies each finding as supported or skipped. Supported findings produce a `PlannedChange` — the path, the field, the new value, and the source document's hash recorded at plan time. Skipped findings carry a reason: `unsupported`, `ambiguous`, `missing_hash`, or `precondition_failed`.\n\nA planned change:\n\n{\n  \"path\": \"notes/welcome.md\",\n  \"field\": \"kind\",\n  \"new_value\": \"note\",\n  \"document_hash\": \"a3f2…\"\n}\n\nA skipped finding records the reason:\n\n{\n  \"path\": \"drafts/x.md\",\n  \"code\": \"link-ambiguous\",\n  \"skip_reason\": \"ambiguous\"\n}\n\nThe plan captures a vault snapshot. Each change records the document's hash at plan time; apply refuses to write if that hash has changed. Re-run plan after editing files between plan and apply.\n\nTriage filters here are the same as on `validate` — pass `--severity error` to plan only error-level findings. Filters that excluded a finding from validate also exclude it from plan.",
        )],
        "vault repair apply" => &[(
            "How apply writes",
            "Apply walks the plan in this order:\n\n1. Load the plan JSON and verify its schema version.\n2. Confirm the plan's recorded vault root matches the effective cwd.\n3. Re-read each source document and verify its hash matches what the plan recorded; abort if any file changed since plan time.\n4. Verify each `expected_old_value` matches the current field value; abort on mismatch.\n5. Write the new frontmatter, preserving the Markdown body.\n6. Re-run validate when `--verify` is set.\n\nPass `--dry-run` to walk steps 1–4 without writing.",
        )],
        _ => &[],
    };
    pairs
        .iter()
        .map(|(heading, body)| (heading.to_string(), body.to_string()))
        .collect()
}

/// Map a command path to its live-examples generator, if any. Phase 3 wires
/// `vault find`; everything else returns `None` and the LIVE EXAMPLES block
/// is omitted at render time.
///
/// The generator (when present) is invoked by the help interceptor on
/// `--help` form only, after `Cache::open` succeeds.
pub fn live_examples_fn_for(cmd_path: &str) -> Option<fn(&vault_cache::Cache) -> Vec<LiveExample>> {
    match cmd_path {
        "vault find" => Some(crate::help::find_live::live_examples_for_find),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_path_returns_empty() {
        assert!(examples_for("vault nonexistent").is_empty());
    }

    #[test]
    fn root_path_has_examples() {
        assert!(!examples_for("vault").is_empty());
    }

    #[test]
    fn find_path_has_examples() {
        let ex = examples_for("vault find");
        assert!(!ex.is_empty());
        // At least one example should demonstrate the `--eq` predicate.
        assert!(ex.iter().any(|(cmd, _)| cmd.contains("--eq")));
    }

    #[test]
    fn conceptual_sections_for_unknown_path_returns_empty() {
        assert!(conceptual_sections_for("vault nonexistent").is_empty());
    }

    #[test]
    fn validate_has_how_validation_works_section() {
        let sections = conceptual_sections_for("vault validate");
        assert!(
            sections.iter().any(|(h, _)| h == "How validation works"),
            "expected `How validation works` section; got headings: {:?}",
            sections.iter().map(|(h, _)| h).collect::<Vec<_>>()
        );
    }

    #[test]
    fn repair_plan_has_plan_apply_boundary_section() {
        let sections = conceptual_sections_for("vault repair plan");
        assert!(
            sections.iter().any(|(h, _)| h == "The plan/apply boundary"),
            "expected `The plan/apply boundary` section; got headings: {:?}",
            sections.iter().map(|(h, _)| h).collect::<Vec<_>>()
        );
    }

    #[test]
    fn repair_apply_has_how_apply_writes_section() {
        let sections = conceptual_sections_for("vault repair apply");
        assert!(
            sections.iter().any(|(h, _)| h == "How apply writes"),
            "expected `How apply writes` section; got headings: {:?}",
            sections.iter().map(|(h, _)| h).collect::<Vec<_>>()
        );
    }

    #[test]
    fn repair_plan_section_mentions_supported_and_skipped() {
        let sections = conceptual_sections_for("vault repair plan");
        let (_, body) = sections
            .iter()
            .find(|(h, _)| h == "The plan/apply boundary")
            .expect("boundary section present");
        assert!(body.contains("supported"));
        assert!(body.contains("skipped"));
    }

    #[test]
    fn validate_has_finding_codes_section() {
        let sections = conceptual_sections_for("vault validate");
        assert!(
            sections.iter().any(|(h, _)| h == "Finding codes"),
            "expected `Finding codes` section; got headings: {:?}",
            sections.iter().map(|(h, _)| h).collect::<Vec<_>>()
        );
    }

    #[test]
    fn validate_finding_codes_section_lists_all_ten_codes() {
        let sections = conceptual_sections_for("vault validate");
        let body = sections
            .iter()
            .find(|(h, _)| h == "Finding codes")
            .map(|(_, b)| b.clone())
            .expect("Finding codes section present");
        for code in [
            "link-unresolved",
            "link-ambiguous",
            "frontmatter-required-field-missing",
            "frontmatter-disallowed-value",
            "frontmatter-invalid-type",
            "frontmatter-forbidden-field",
            "frontmatter-alias-shadowed-by-stem",
            "frontmatter-alias-duplicate-across-docs",
            "frontmatter-alias-malformed",
            "document-misrouted",
        ] {
            assert!(
                body.contains(code),
                "expected code `{code}` in Finding codes body; got:\n{body}"
            );
        }
    }

    #[test]
    fn repair_apply_section_is_a_numbered_sequence() {
        let sections = conceptual_sections_for("vault repair apply");
        let (_, body) = sections
            .iter()
            .find(|(h, _)| h == "How apply writes")
            .expect("apply section present");
        // The acceptance criterion is a numbered list; verify the first few
        // items render as `1.`, `2.`, `3.` so we don't regress to bullets or
        // prose.
        for needle in ["1.", "2.", "3."] {
            assert!(
                body.contains(needle),
                "expected numbered item {needle:?}; got body:\n{body}"
            );
        }
    }
}
