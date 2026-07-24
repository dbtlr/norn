//! Hand-authored canned `--help` examples per command path.
//!
//! Per the Phase 2 design spec, examples are biased toward fewer — empty is
//! the correct answer for many commands. The match below returns `vec![]`
//! for any command path with no authored examples; the renderer skips the
//! `EXAMPLES` section when the table is empty.
//!
//! Each entry is `(command_line, comment)`. Comments are ≤60 chars, lowercase
//! except for required literals, no trailing period. The command line uses
//! the literal `norn` prefix; the renderer styles tokens per palette.

use crate::help::plan_example::render_plan_example;

/// Return canned examples for the given command path string (e.g. `"norn find"`).
///
/// Returns `vec![]` for unknown paths and for paths intentionally without
/// examples. The renderer's `EXAMPLES` section is suppressed when the table
/// is empty.
pub fn examples_for(cmd_path: &str) -> Vec<(String, String)> {
    let pairs: &[(&str, &str)] = match cmd_path {
        "norn" => &[
            (
                "norn find --eq type:note --limit 5",
                "5 notes by default sort",
            ),
            (
                "norn validate --format json",
                "machine-readable validation findings",
            ),
            (
                "norn repair --plan --out plan.json",
                "generate a MigrationPlan",
            ),
        ],
        "norn find" => &[
            (
                "norn find --eq type:note --limit 5",
                "5 notes; default sort",
            ),
            (
                "norn find --text reorg --format paths",
                "full-text search; pipe-friendly paths",
            ),
            (
                "norn find --has aliases --col title,aliases",
                "docs that declare aliases",
            ),
            (
                "norn find --in type:note,log --sort modified --desc",
                "two types, newest first",
            ),
            (
                "norn find --eq type:task --links-to norn --format paths",
                "tasks that link to a doc; pipe-friendly paths",
            ),
        ],
        "norn count" => &[
            ("norn count", "total document count in the vault"),
            (
                "norn count --eq type:note --by status",
                "notes only, grouped by status",
            ),
            (
                "norn count --path 'Workspaces/**/tasks/*.md' --by status",
                "one project's tasks, grouped by status",
            ),
        ],
        "norn show" => &[
            ("norn show foo", "show one doc by case-insensitive stem"),
            (
                "norn show '[[foo]]'",
                "wikilink input; anchor/alias suffixes stripped",
            ),
            (
                "norn show foo --col incoming_links",
                "backlinks only (the absorbed `links backlinks` job)",
            ),
            (
                "norn show a.md b.md c.md",
                "multiple targets, one record per doc",
            ),
        ],
        "norn validate" => &[
            (
                "norn validate",
                "human-readable findings on the configured vault",
            ),
            (
                "norn validate --format json",
                "machine-readable findings for pipelines",
            ),
            (
                "norn validate --severity error",
                "errors only; skip warnings",
            ),
            ("norn validate --code 'link-*'", "broken + ambiguous links"),
            (
                "norn validate --code 'link-*' --format paths",
                "unique source paths only; pipe-friendly",
            ),
        ],
        "norn repair" => &[
            ("norn repair", "summarize repairable findings; no writes"),
            (
                "norn repair --plan --out plan.json",
                "write a MigrationPlan to file",
            ),
            (
                "norn repair --plan --format json | norn apply -",
                "generate a plan and apply it through apply",
            ),
            (
                "norn repair --plan --format paths",
                "affected paths only; pipe to xargs",
            ),
            (
                "norn repair --plan --skip-reason ambiguous-target",
                "show only ambiguous-target skips",
            ),
            (
                "norn repair --plan --severity error",
                "plan only error-level findings",
            ),
        ],
        // ── Default tier: 1-2 examples each ─────────────────────────────────
        "norn init" => &[(
            "norn init",
            "scaffold .norn/config.yaml in the current directory",
        )],
        "norn config show" => &[
            ("norn config show", "effective config: paths + counts"),
            (
                "norn config show --format json",
                "machine-readable config for pipelines",
            ),
        ],
        "norn cache rebuild" => &[(
            "norn cache rebuild",
            "delete and rebuild the cache from scratch",
        )],
        "norn cache status" => &[("norn cache status", "cache path, size, doc and link counts")],
        "norn cache index" => &[
            (
                "norn cache index",
                "incremental refresh via mtime+size check",
            ),
            (
                "norn cache index --force-hash",
                "hash every file; bypass cheap-check",
            ),
        ],
        "norn repair links" => &[
            (
                "norn repair links",
                "report link and path repair risks; no writes",
            ),
            (
                "norn repair links --target old.md --move-to new.md",
                "preview link risk if target were moved",
            ),
        ],
        "norn new" => &[
            (
                "norn new Workspaces/my-project/tasks/2026-05-26-design-foo.md --yes",
                "create a task doc; schema defaults fill required frontmatter",
            ),
            (
                "norn new notes/my-note.md --field description=\"Design pass\" --yes",
                "override one field; remaining defaults come from the matched rule",
            ),
            (
                "norn new Inbox/draft.md --parents --yes",
                "--parents creates missing ancestor dirs (mkdir -p style)",
            ),
            (
                "norn new notes/my-note.md --dry-run",
                "preview the scaffold and defaults without writing",
            ),
        ],
        "norn set" => &[
            (
                "norn set notes/project.md --field status=active --yes",
                "set the `status` field to `active`; skip confirm",
            ),
            (
                "norn set notes/project.md --push aliases=new-alias --yes",
                "append a value to an array-typed frontmatter field",
            ),
            (
                "norn set notes/project.md --pop aliases=old-name --yes",
                "drop a value from an array; silent if absent",
            ),
            (
                "norn set notes/project.md --remove priority --yes",
                "remove a frontmatter key (blocks on schema required fields)",
            ),
            (
                r#"echo "new body" | norn set notes/project.md --body-from-stdin --yes"#,
                "wholesale replace body content via stdin; frontmatter kept",
            ),
            (
                r#"norn set notes/project.md --field-json tags='["foo","bar"]' --yes"#,
                "set a structured value via raw JSON; escape hatch",
            ),
            (
                "norn set notes/project.md --field status=active --dry-run --format json",
                "preview the mutation as JSON without writing",
            ),
        ],

        // ── Thin tier: 0-1 examples each ────────────────────────────────────
        "norn completions init" => &[(
            "norn completions init zsh",
            "emit zsh completion script to stdout",
        )],
        "norn completions install" => &[(
            "norn completions install",
            "install for the shell detected from $SHELL",
        )],

        // Thin commands without arms (intentionally empty — flag block self-explains):
        // norn cache clear, norn config validate,
        // norn config migrate, norn config edit
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
///
/// Intent-verb commands (move, delete, set, new, rewrite-wikilink) include a
/// PLAN OPERATION section showing the MigrationPlan YAML equivalent so users
/// and agents can see CLI ⇄ plan parity at the point of consumption.
pub fn conceptual_sections_for(cmd_path: &str) -> Vec<(String, String)> {
    // Intent-verb PLAN OPERATION blocks: show the MigrationPlan YAML
    // equivalent for each command so the CLI ⇄ plan parity is visible
    // in --help output.
    match cmd_path {
        "norn move" => {
            return vec![(
                "Plan operation".to_string(),
                render_plan_example("move_document", &[("src", "<SRC>"), ("dst", "<DST>")]),
            )];
        }
        "norn delete" => {
            return vec![(
                "Plan operation".to_string(),
                render_plan_example("delete_document", &[("path", "<DOC>")]),
            )];
        }
        "norn set" => {
            return vec![(
                "Plan operation".to_string(),
                render_plan_example(
                    "set_frontmatter",
                    &[
                        ("path", "<DOC>"),
                        ("field", "<KEY>"),
                        ("new_value", "<VALUE>"),
                    ],
                ),
            )];
        }
        "norn new" => {
            return vec![(
                "Plan operation".to_string(),
                render_plan_example("create_document", &[("path", "<PATH>")]),
            )];
        }
        "norn rewrite-wikilink" => {
            return vec![(
                "Plan operation".to_string(),
                render_plan_example("rewrite_wikilink", &[("old", "<OLD>"), ("new", "<NEW>")]),
            )];
        }
        _ => {}
    }

    let pairs: &[(&str, &str)] = match cmd_path {
        "norn validate" => &[
            (
                "How validation works",
                "Validate reads `.norn/config.yaml` for the rules that shape your vault: required frontmatter fields, allowed values, expected types, and path scoping. Each rule produces findings with a stable code and a severity (`error`, `warning`, `info`).\n\nFindings cover three surfaces. Frontmatter findings come from schema rules — codes like `frontmatter-required-field-missing` and `value-not-allowed`. Link findings come from graph facts — `link-target-missing`, `link-anchor-missing`, `link-block-missing`, and `link-ambiguous`. Document diagnostics come from parse — malformed frontmatter, encoding issues. Validate never writes files.\n\nExit code is `1` when any finding has severity `error`, `0` otherwise. Pipelines gate on this exit code.\n\nTriage filters combine with AND across types and OR within a type. `--severity error --code frontmatter-required-field-missing` returns errors that match that code. `--code 'link-*'` returns the whole family. `--path 'notes/**'` scopes to a path glob; `--field`, `--rule`, `--target`, and `--reason` narrow further.",
            ),
            (
                "Finding codes",
                "Codes identify validation findings. Filter with --code <code>. Glob patterns supported (--code 'link-*').\n\nlink-target-missing         A wikilink target doesn't exist in the vault.\nlink-anchor-missing         The target exists but the #anchor isn't present.\nlink-block-missing          The target exists but the ^block-ref isn't present.\nlink-ambiguous              A wikilink resolves to multiple candidates.\nfrontmatter-required-field-missing\n                            A required frontmatter field is absent.\nvalue-not-allowed           A field's value is not in the configured set.\nfield-type-invalid          A field's value doesn't match its declared type.\nfrontmatter-exceeds-max-length\n                            A string/list_of_strings field matches its type but exceeds max_length.\nfrontmatter-forbidden-field A field that the rule forbids is present.\nfrontmatter-alias-shadowed-by-stem\n                            An alias matches another doc's stem; the alias is dead because stem resolution wins.\nfrontmatter-alias-duplicate-across-docs\n                            Two or more docs claim the same alias; wikilinks resolving via that alias will be ambiguous.\nfrontmatter-alias-malformed The alias field contains a non-scalar value.\ndocument-misrouted          A doc is in a directory the rule's path selector excludes.\nfrontmatter-reference-type  A field's wikilink resolves to a document whose type is outside the allowed set.",
            ),
        ],
        "norn repair" => &[(
            "The plan/apply boundary",
            "Repair runs in two halves. `norn repair --plan` reads validate findings and emits a MigrationPlan JSON artifact describing every change it would make. Planning never writes to vault documents. `norn apply` consumes that artifact and writes the changes; preconditions are checked before any file is touched.\n\nPlanning classifies each finding as supported or skipped. Supported findings produce a typed operation in the plan's `operations` list — its `kind` (e.g. `set_frontmatter`) plus a `fields` object carrying the path, the field, the new value, and the source document's hash recorded at plan time. Skipped findings carry a reason code (stable kebab-case string): `missing-default`, `link-decision-needed`, `no-rule-matched`, `alias-shadowed`, `graph-diagnostic`, `ambiguous-target`, `missing-hash`, or `precondition-failed`. Filter skipped findings with `--skip-reason <PATTERN>`; glob patterns accepted.\n\nA plan operation:\n\n{\n  \"kind\": \"set_frontmatter\",\n  \"fields\": {\n    \"path\": \"notes/welcome.md\",\n    \"field\": \"kind\",\n    \"new_value\": \"note\",\n    \"document_hash\": \"a3f2…\"\n  }\n}\n\nA skipped finding records the reason (the `reason` value is the kebab-case skip-reason code; `finding_code` is the underlying validate code):\n\n{\n  \"finding_code\": \"link-ambiguous\",\n  \"path\": \"drafts/x.md\",\n  \"reason\": \"ambiguous-target\"\n}\n\nThe summary's `skipped` section uses a `by_reason` map: `{ \"ambiguous-target\": 3, \"no-rule-matched\": 12 }`. Zero-count buckets are omitted.\n\nOutput formats: `--format records` (human summary, TTY default), `--format json` (full MigrationPlan envelope, pipe default), `--format paths` (one affected path per line, deduplicated).\n\nThe plan captures a vault snapshot. Each change records the document's hash at plan time; apply refuses to write if that hash has changed. Re-run `--plan` after editing files between plan and apply.\n\nTriage filters here are the same as on `validate` — pass `--severity error` to plan only error-level findings. Filters that excluded a finding from validate also exclude it from plan.",
        )],
        "norn apply" => &[(
            "How apply writes",
            "Apply walks the MigrationPlan in this order:\n\n1. Load the plan JSON and verify its schema version.\n2. Confirm the plan's recorded vault root matches the effective cwd.\n3. Re-read each source document and verify its hash matches what the plan recorded; abort if any file changed since plan time.\n4. Verify each `expected_old_value` matches the current field value; abort on mismatch.\n5. Write the new frontmatter, preserving the Markdown body.\n6. Re-run validate when `--verify` is set.\n\nPass `--dry-run` to walk steps 1–4 without writing.",
        )],
        _ => &[],
    };
    pairs
        .iter()
        .map(|(heading, body)| (heading.to_string(), body.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_path_returns_empty() {
        assert!(examples_for("norn nonexistent").is_empty());
    }

    #[test]
    fn root_path_has_examples() {
        assert!(!examples_for("norn").is_empty());
    }

    #[test]
    fn find_path_has_examples() {
        let ex = examples_for("norn find");
        assert!(!ex.is_empty());
        // At least one example should demonstrate the `--eq` predicate.
        assert!(ex.iter().any(|(cmd, _)| cmd.contains("--eq")));
    }

    #[test]
    fn conceptual_sections_for_unknown_path_returns_empty() {
        assert!(conceptual_sections_for("norn nonexistent").is_empty());
    }

    #[test]
    fn validate_has_how_validation_works_section() {
        let sections = conceptual_sections_for("norn validate");
        assert!(
            sections.iter().any(|(h, _)| h == "How validation works"),
            "expected `How validation works` section; got headings: {:?}",
            sections.iter().map(|(h, _)| h).collect::<Vec<_>>()
        );
    }

    #[test]
    fn repair_has_plan_apply_boundary_section() {
        let sections = conceptual_sections_for("norn repair");
        assert!(
            sections.iter().any(|(h, _)| h == "The plan/apply boundary"),
            "expected `The plan/apply boundary` section; got headings: {:?}",
            sections.iter().map(|(h, _)| h).collect::<Vec<_>>()
        );
    }

    #[test]
    fn apply_has_how_apply_writes_section() {
        let sections = conceptual_sections_for("norn apply");
        assert!(
            sections.iter().any(|(h, _)| h == "How apply writes"),
            "expected `How apply writes` section; got headings: {:?}",
            sections.iter().map(|(h, _)| h).collect::<Vec<_>>()
        );
    }

    #[test]
    fn repair_section_mentions_supported_and_skipped() {
        let sections = conceptual_sections_for("norn repair");
        let (_, body) = sections
            .iter()
            .find(|(h, _)| h == "The plan/apply boundary")
            .expect("boundary section present");
        assert!(body.contains("supported"));
        assert!(body.contains("skipped"));
    }

    #[test]
    fn validate_has_finding_codes_section() {
        let sections = conceptual_sections_for("norn validate");
        assert!(
            sections.iter().any(|(h, _)| h == "Finding codes"),
            "expected `Finding codes` section; got headings: {:?}",
            sections.iter().map(|(h, _)| h).collect::<Vec<_>>()
        );
    }

    #[test]
    fn validate_finding_codes_section_lists_all_stable_codes() {
        let sections = conceptual_sections_for("norn validate");
        let body = sections
            .iter()
            .find(|(h, _)| h == "Finding codes")
            .map(|(_, b)| b.clone())
            .expect("Finding codes section present");
        for code in [
            "link-target-missing",
            "link-anchor-missing",
            "link-block-missing",
            "link-ambiguous",
            "frontmatter-required-field-missing",
            "value-not-allowed",
            "field-type-invalid",
            "frontmatter-exceeds-max-length",
            "frontmatter-forbidden-field",
            "frontmatter-alias-shadowed-by-stem",
            "frontmatter-alias-duplicate-across-docs",
            "frontmatter-alias-malformed",
            "frontmatter-reference-type",
            "document-misrouted",
        ] {
            assert!(
                body.contains(code),
                "expected code `{code}` in Finding codes body; got:\n{body}"
            );
        }
    }

    #[test]
    fn apply_section_is_a_numbered_sequence() {
        let sections = conceptual_sections_for("norn apply");
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

    #[test]
    fn set_examples_block_is_present() {
        let examples = examples_for("norn set");
        assert!(!examples.is_empty(), "norn set should have EXAMPLES");
    }

    #[test]
    fn set_examples_cover_field_push_pop_remove_body_json_and_dryrun() {
        let examples = examples_for("norn set");
        let body = examples
            .iter()
            .map(|(cmd, _)| cmd.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.contains("--field"), "should have --field example");
        assert!(body.contains("--push"), "should have --push example");
        assert!(body.contains("--pop"), "should have --pop example");
        assert!(body.contains("--remove"), "should have --remove example");
        assert!(
            body.contains("--body-from-stdin"),
            "should have --body-from-stdin example"
        );
        assert!(
            body.contains("--field-json"),
            "should have --field-json example"
        );
        assert!(body.contains("--dry-run"), "should have --dry-run example");
    }
}
