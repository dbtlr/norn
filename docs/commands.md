---
title: Command reference
description: One-line reference for every norn subcommand with the canonical invocation and a link to deeper material.
---

# Command reference

Every command accepts the global flags below and a per-command `--format` where applicable. Run `norn <command> --help` for the authoritative flag list.

## Global flags

| Flag | Description |
|---|---|
| `-C, --cwd <dir>` | Run against `<dir>` instead of the process current directory. |
| `--config <path>` | Explicit `.norn/config.yaml` path. Relative paths resolve against the effective cwd. |
| `--verbose` | Verbose stderr logging. |

When `--config` is omitted, `norn` discovers `<cwd>/.norn/config.yaml` if it exists; missing discovered config is fine and uses defaults.

## find

Document search across paths, frontmatter, and body text. Requires at least one predicate or `--all`.

```bash
norn find --all --format records
norn find --eq status:draft --format jsonl
norn find --path "notes/**/*.md" --has tags --format paths
norn find --text "literal substring" --format paths
```

Predicates: `--text`, `--eq`, `--not-eq`, `--in`, `--not-in`, `--has`, `--missing`, `--before`, `--after`, `--on`, `--path`. All predicates are ANDed; comma-separated values within `--in`/`--not-in` are ORed.

## count

Grouped document counts for a frontmatter field. Shares the full filter flag surface with `norn find`.

```bash
norn count --by status --format json
norn count --by type --eq status:draft --format text
norn count --format json
```

Without `--by`, emits the total document count only.

## get

Single-document detail: frontmatter, headings, outgoing links, unresolved links, incoming links. Accepts vault-relative paths, case-insensitive stems, and wikilink-shaped inputs.

```bash
norn get "My Note" --format json
norn get "notes/my-note.md" --format json
norn get "My Note" --col incoming_links --format jsonl
norn get "My Note" --body --format json
```

`--col` narrows the output fields; `--body` adds document body content. Multiple targets are accepted.

## validate

Read-only validation against configured rules.

```bash
norn validate --format jsonl
norn validate --summary --format records
norn validate --code frontmatter-invalid-type --field created --format jsonl
norn validate --rule task-status --path "notes/**/*.md" --summary --format json
```

Filter flags: `--code`, `--severity`, `--field`, `--rule`, `--path`, `--target`, `--reason`. Comma-separated values within one filter are ORed; different filters are ANDed. Filters apply to both raw output and `--summary`.

See [validation.md](validation.md) for finding codes, summary shape, and recipes.

## repair plan

Read-only repair planning. Produces a JSON plan artifact for review.

```bash
norn repair plan --format json > repair.json
norn repair plan --out repair.json
norn repair plan --code frontmatter-disallowed-value --field status --out repair.json
```

The plan has `schema_version`, `vault_root`, `source_filters`, `summary`, `changes`, and `skipped_findings`. See [validation.md](validation.md).

Output formats:

- `--format report` (TTY default) — decision-support summary with counts, skip tally, top affected files, and inline apply guidance.
- `--format json` (pipe default) — full envelope artifact, the only format `norn repair apply` consumes.
- `--format paths` — affected document paths, one per line, sorted and deduplicated.

(Note: `--format jsonl` and `--format table` were removed in v0.32; both are rejected with migration messages.)

## repair apply

Apply a repair plan. Writes by default; pass `--dry-run` to preview.

Plan ingress: positional path, `-` for stdin, or omit the positional to read from stdin. The pipeline form `norn repair plan --format json | norn repair apply` composes plan generation and apply in one shot.

```bash
norn repair apply repair.json
norn repair apply repair.json --dry-run
norn repair plan --format json | norn repair apply --dry-run
norn repair apply repair.json --verify
norn repair apply repair.json --out report.json
```

Output formats:

- `--format report` (TTY default) — human summary: count line, severity tally, by-operation breakdown, optional warnings sub-block, footer with totals and next-step hint.
- `--format json` (pipe default) — full `RepairApplyReport` envelope (`schema_version`, `dry_run`, `changed_files`, `applied_changes`, `moved_files`, `rewritten_links`, `warnings`, `plan_context`, optional `verification`).
- `--format paths` — sorted dedup of `changed_files`, one per line. Empty (zero bytes) when no files changed.

`--out <PATH>` writes the JSON report to file independently of `--format`; stdout stays silent when `--out` is set without `--format`. When both are set, both streams are honored.

(Note: `--format jsonl` and `--format table` were removed in v0.32; both are rejected with migration messages.)

Apply rejects mismatched vault roots, stale document hashes, unsupported schema versions, conflicting field changes, and expected-old-value mismatches. The orchestrator is atomic-at-batch-level: any precondition failure aborts the whole apply before any partial writes (stderr error, exit 1, no report rendered).

## set

Update one document — frontmatter mutation and wholesale body replacement.

```bash
norn set notes/task.md --field status=active
norn set notes/task.md --push tags=work --dry-run
norn set notes/task.md --remove old_key --yes
norn set notes/task.md --field-json count=42 --format json
echo "new body" | norn set notes/task.md --body-from-stdin --yes
```

Flag classes:

| Flag | Purpose |
|---|---|
| `--field KEY=VALUE` | Set a frontmatter field. Repeatable; multiple instances of the same key accumulate into an array. |
| `--field-json KEY=JSON` | Set a frontmatter field with an explicitly JSON-parsed value (arrays, objects, null). |
| `--push KEY=VALUE` | Append a value to a list-typed field. Creates a single-element array if the key is absent. |
| `--pop KEY=VALUE` | Remove a value from a list-typed field. Silent no-op if the value is absent. |
| `--remove KEY` | Drop a frontmatter key entirely. Silent no-op if absent. |
| `--body-from-stdin` | Wholesale body replacement; new body content is read from stdin. |
| `--force` | Bypass schema enforcement (type validation and required-field protection). |
| `--yes` | Skip the interactive confirmation prompt and apply. |
| `--dry-run` | Preview the mutation without writing. |
| `--format records\|json` | Output shape. `json` emits the SetReport envelope. |

Schema-aware behavior: when `field_types` rules are configured, `norn set` validates
each value's type before applying. `--force` bypasses type validation and required-field
protection. `--remove` on a required field is refused unless `--force` is given.

Wikilink fields: when a field is declared `wikilink` or `wikilink_or_list` in `field_types`,
values are auto-wrapped on write (`norn` becomes `[[norn]]`). Unresolved or
ambiguous link targets surface as warnings (not refusals).

Atomicity: all ops in a single invocation apply as one filesystem write. Any pre-flight
refusal is all-or-nothing — no partial writes.

Apply model: matches `norn move` and `norn delete`. TTY shows a preview and prompts for
confirmation; non-TTY without `--yes` prints a dry-run summary and exits. `--yes` skips the
prompt and applies. `--dry-run` previews and exits. `--format json` is implicitly
non-interactive and emits the SetReport envelope.

Output: SetReport JSON envelope with `schema_version: 1`.

Exit codes: 0 success or dry-run, 1 operator-cancelled, 2 pre-flight refusal.

## new

Create a document. Fills frontmatter from `frontmatter_defaults` declared in the
matching validate rule; the path drives substitution variables (`{{title}}`,
`{{date}}`, `{{path.X}}`, and the full Norn transform set).

```bash
norn new notes/2026-05-26-design-foo.md --yes
norn new notes/my-note.md --field description="Design pass" --yes
norn new Inbox/draft.md --parents --yes
norn new notes/my-note.md --dry-run
```

Flags: `--field KEY=VALUE` (override a default), `--parents` / `-p` (create
missing ancestor directories), `--dry-run` (preview without writing), `--yes`
(skip confirm prompt), `--format records|json`.

Apply model: same safe-by-default pattern as `norn set`, `move`, and `delete`.
TTY shows a preview and prompts; non-TTY without `--yes` dry-runs. Post-create
`norn validate` runs automatically; findings surface as envelope warnings.

## Document mutation surface

`norn new`, `norn get`, `norn set`, `norn move`, and `norn delete` form a
CRUD-shaped surface for working with vault documents without touching the
filesystem directly.
All mutation commands (`set`, `new`, `move`, `delete`) are safe-by-default: TTY runs
prompt for confirmation, non-TTY runs without `--yes` print a dry-run summary
and exit. `--yes` skips the prompt and applies; `--dry-run` previews and
exits explicitly; `--format json` is implicitly non-interactive.

The cascading backlink rewrites that `norn move` performs reuse the
existing `apply_link_rewrites` machinery from the repair-apply orchestrator;
`norn delete --rewrite-to <ALT>` does the same for redirecting backlinks
before deletion. Under the hood, `norn set --body-from-stdin` emits a
`replace_body` plan op alongside its frontmatter ops — this is a `norn set`
implementation detail, not a config-rule-triggerable action.

## move

Move or rename a document with cascading backlink rewrites.

```bash
norn move Inbox/task.md Projects/my-project/tasks/task.md
norn move Inbox/task.md Projects/my-project/tasks/task.md --dry-run
norn move Inbox/task.md Projects/my-project/tasks/task.md --yes --format json
```

Flags: `--dry-run` (preview, no write), `--yes` (skip confirm prompt), `--no-link-rewrite` (move file only), `--force` (overwrite destination).

## delete

Delete a document. Refuses if incoming links exist unless `--allow-broken-links` or `--rewrite-to` is supplied.

```bash
norn delete notes/old-note.md --dry-run
norn delete notes/old-note.md --allow-broken-links --yes
norn delete notes/old-note.md --rewrite-to notes/replacement.md --yes
```

Flags: `--dry-run`, `--yes`, `--allow-broken-links`, `--rewrite-to <ALT>`.

To audit link drift before moving or deleting: `norn validate --code 'link-*'` surfaces unresolved and ambiguous links across the vault.

## cache

Cache management subcommands. See [cache.md](cache.md) for full documentation.

| Command | Purpose |
|---|---|
| `norn cache index` | Update the cache incrementally (default). |
| `norn cache index --rebuild` | Full rebuild from scratch. |
| `norn cache index --force-hash` | Skip mtime cheap-check; hash every file. |
| `norn cache rebuild` | Alias for `cache index --rebuild`. |
| `norn cache clear` | Delete the cache; next command rebuilds. |
| `norn cache status` | Report cache path, size, doc/link/file counts, schema version. |

Query commands (`norn validate`, `norn find`, `norn count`, `norn get`, `norn repair`) refresh the cache implicitly before reading. Pass the global `--no-cache-refresh` flag to skip that step.

## Hidden subcommands

`norn` also exposes hidden subcommands for shell completions and the man page (used by the installer). These don't appear in `norn --help` top-level output:

```bash
norn completions bash > norn.bash
norn completions zsh  > _norn
norn completions fish > norn.fish
norn manpage          > norn.1
```

They are added in Slice 4 of the v0.26 GitHub-readiness work.

## See also

- [Configuration](configuration.md) — every config key.
- [Validation and repair](validation.md) — finding codes and recipes.
- [Agent workflows](agent-workflows.md) — the stable JSON/JSONL contracts.
