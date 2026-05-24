---
title: Command reference
description: One-line reference for every vault subcommand with the canonical invocation and a link to deeper material.
---

# Command reference

Every command accepts the global flags below and a per-command `--format` where applicable. Run `vault <command> --help` for the authoritative flag list.

## Global flags

| Flag | Description |
|---|---|
| `-C, --cwd <dir>` | Run against `<dir>` instead of the process current directory. |
| `--config <path>` | Explicit `.vault/config.yaml` path. Relative paths resolve against the effective cwd. |
| `--verbose` | Verbose stderr logging. |

When `--config` is omitted, `vault` discovers `<cwd>/.vault/config.yaml` if it exists; missing discovered config is fine and uses defaults.

## find

Document search across paths, frontmatter, and body text. Requires at least one predicate or `--all`.

```bash
vault find --all --format records
vault find --eq status:draft --format jsonl
vault find --path "notes/**/*.md" --has tags --format paths
vault find --text "literal substring" --format paths
```

Predicates: `--text`, `--eq`, `--not-eq`, `--in`, `--not-in`, `--has`, `--missing`, `--before`, `--after`, `--on`, `--path`. All predicates are ANDed; comma-separated values within `--in`/`--not-in` are ORed.

## count

Grouped document counts for a frontmatter field. Shares the full filter flag surface with `vault find`.

```bash
vault count --by status --format json
vault count --by type --eq status:draft --format text
vault count --format json
```

Without `--by`, emits the total document count only.

## show

Single-document detail: frontmatter, headings, outgoing links, unresolved links, incoming links. Accepts vault-relative paths, case-insensitive stems, and wikilink-shaped inputs.

```bash
vault show "My Note" --format json
vault show "notes/my-note.md" --format json
vault show "My Note" --col incoming_links --format jsonl
vault show "My Note" --body --format json
```

`--col` narrows the output fields; `--body` adds document body content. Multiple targets are accepted.

## validate

Read-only validation against configured rules.

```bash
vault validate --format jsonl
vault validate --summary --format records
vault validate --code frontmatter-invalid-type --field created --format jsonl
vault validate --rule task-status --path "notes/**/*.md" --summary --format json
```

Filter flags: `--code`, `--severity`, `--field`, `--rule`, `--path`, `--target`, `--reason`. Comma-separated values within one filter are ORed; different filters are ANDed. Filters apply to both raw output and `--summary`.

See [validation.md](validation.md) for finding codes, summary shape, and recipes.

## repair plan

Read-only repair planning. Produces a JSON plan artifact for review.

```bash
vault repair plan --format json > repair.json
vault repair plan --out repair.json
vault repair plan --code frontmatter-disallowed-value --field status --out repair.json
```

The plan has `schema_version`, `vault_root`, `source_filters`, `summary`, `changes`, and `skipped_findings`. See [validation.md](validation.md).

## repair apply

Apply a repair plan. Writes by default; pass `--dry-run` to preview.

```bash
vault repair apply repair.json --dry-run --format json
vault repair apply repair.json --verify --format json
```

Apply rejects mismatched vault roots, stale document hashes, unsupported schema versions, conflicting field changes, and expected-old-value mismatches.

## repair links

Read-only link/path planning report. Does not rewrite links or move files.

```bash
vault repair links --format json
vault repair links --target "notes/some-note.md" --format json
vault repair links --target "some-note" --format table
```

The report includes unresolved links, ambiguous links, path-style Markdown links worth reviewing before path moves, duplicate-stem risks, and optional move/delete risk for a `--target`.

### `vault repair links --target <path> --move-to <destination>`

Read-only analysis of what would change if the target were moved to the destination. Produces the same `link_risk` + `warnings` shape as the planner would compute for an authored `move_document` repair rule, without writing a plan file.

```bash
vault repair links --target Inbox/task.md --move-to Workspaces/demo/tasks/task.md --format json
```

Output includes a `link_risk` object (stem-only, path-qualified, and Markdown backlinks with their precomputed rewrites) and any planner warnings (e.g., `StemCollisionAfterMove`).

## cache

Cache management subcommands. See [cache.md](cache.md) for full documentation.

| Command | Purpose |
|---|---|
| `vault cache index` | Update the cache incrementally (default). |
| `vault cache index --rebuild` | Full rebuild from scratch. |
| `vault cache index --force-hash` | Skip mtime cheap-check; hash every file. |
| `vault cache rebuild` | Alias for `cache index --rebuild`. |
| `vault cache clear` | Delete the cache; next command rebuilds. |
| `vault cache status` | Report cache path, size, doc/link/file counts, schema version. |

Query commands (`vault validate`, `vault find`, `vault count`, `vault show`, `vault repair`) refresh the cache implicitly before reading. Pass the global `--no-cache-refresh` flag to skip that step.

## Hidden subcommands

`vault` also exposes hidden subcommands for shell completions and the man page (used by the installer). These don't appear in `vault --help` top-level output:

```bash
vault completions bash > vault.bash
vault completions zsh  > _vault
vault completions fish > vault.fish
vault manpage          > vault.1
```

They are added in Slice 4 of the v0.26 GitHub-readiness work.

## See also

- [Configuration](configuration.md) — every config key.
- [Validation and repair](validation.md) — finding codes and recipes.
- [Agent workflows](agent-workflows.md) — the stable JSON/JSONL contracts.
