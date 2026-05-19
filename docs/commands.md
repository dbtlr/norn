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
| `--vault <name>` | Target a registered vault (see `vault registry`). Mutually exclusive with `-C`. |
| `--config <path>` | Explicit `.vault/config.yaml` path. Relative paths resolve against the effective cwd. |
| `--verbose` | Verbose stderr logging. |

When `--config` is omitted, `vault` discovers `<cwd>/.vault/config.yaml` if it exists; missing discovered config is fine and uses defaults.

## docs

Document inventory and inspection.

| Command | Description | Deep link |
|---|---|---|
| `vault docs list` | List documents with optional path / filter / has / missing filters. | [configuration.md](configuration.md) |
| `vault docs summary --count-by <field>` | Grouped document counts for one frontmatter field. | [concepts.md](concepts.md) |
| `vault docs inspect <path-or-stem>` | Emit one document's parsed shape (frontmatter, headings, outbound links). | [concepts.md](concepts.md) |

Examples:

```bash
vault docs list --format table
vault docs list --filter status:draft --format jsonl
vault docs list --path "notes/**/*.md" --has tags --format paths
vault docs summary --count-by status --format json
vault docs inspect "My Note" --format json
```

## files

Non-Markdown file inventory (and Markdown when you want the file-level view rather than the document-level view).

```bash
vault files --format jsonl
```

## links

Link graph queries.

| Command | Description |
|---|---|
| `vault links list` | Every link the graph contains. |
| `vault links unresolved` | Links that did not resolve to a target. |
| `vault links backlinks <path-or-stem-or-file>` | Incoming links to a target. |

Examples:

```bash
vault links list --format jsonl
vault links unresolved --format jsonl
vault links backlinks "My Note" --format json
vault links backlinks "assets/diagram.png" --format paths
```

`--format paths` emits one unique source path per row.

## search

Document search across paths, frontmatter, and body text.

```bash
vault search --text "literal substring" --format paths
vault search --filter status:draft --format jsonl
vault search --path "notes/**/*.md" --has tags --text "drift" --format json
```

Repeated `--text` values are ANDed. Repeated `--filter` are ANDed; comma-separated values within one filter are ORed.

Search is exact literal substring + frontmatter + path glob. There is no regex, fuzzy, or semantic search.

## validate

Read-only validation against configured rules.

```bash
vault validate --format jsonl
vault validate --summary --format table
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

## registry

Persistent named-vault registry. Stored at `$XDG_CONFIG_HOME/vault/registry.yaml`.

```bash
vault registry add myvault /path/to/vault
vault registry list --format table
vault registry remove myvault
```

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
