---
title: migrate
description: Apply a MigrationPlan — move, delete, rewrite, and frontmatter ops from a plan file.
---

# norn migrate

Apply a `MigrationPlan` — the artifact `norn repair --plan` produces, or a hand-authored one. `migrate` is the command that writes a batch of planned changes; it checks every precondition before touching a file and aborts the whole batch if any check fails.

## Examples

```bash
norn migrate plan.json --dry-run
# walk the plan and check preconditions; no write

norn migrate plan.json --yes
# apply the plan

norn repair --plan --format json | norn migrate -
# generate and apply in one pipeline (- reads the plan from stdin)

norn migrate plan.json --out report.json
# write the JSON ApplyReport to a file
```

## How apply writes

`migrate` walks the plan in order:

1. Load the plan and verify its schema version.
2. Confirm the plan's recorded vault root matches the current vault.
3. Re-read each source document and verify its hash matches what the plan recorded — abort if any file changed since plan time.
4. Verify each `expected_old_value` matches the current field value — abort on mismatch.
5. Write the changes, preserving each document's Markdown body.

`--dry-run` walks steps 1–4 without writing. A precondition failure (hash drift, an `expected_old_value` mismatch, or a failed edit anchor) aborts the apply with an error (stderr, exit 1, no report). Preconditions are verified per operation class before that class writes; the section/body edit ops go further — every target's hash and transform is validated before any edit is written, so the edit batch as a whole either applies or aborts. To re-check the vault after applying, run [`norn validate`](validate.md) as a follow-up.

## Plan input

The positional `<PLAN>` is a path to a JSON or YAML plan; `-` (or omitting it) reads from stdin. Input format auto-detects by extension; pass `--input-format yaml` for a YAML plan on stdin.

## Plan operations

Each plan `operation` has a `kind` and a `fields` object. Alongside the structural ops (`move_document`, `move_folder`, `delete_document`, `rewrite_link`, `create_document`, `replace_body`, `set_frontmatter` / `add_frontmatter` / `remove_frontmatter`), a plan may carry the **section/body edit ops** — the same vocabulary as [`norn edit`](edit.md): `str_replace`, `replace_section`, `append_to_section`, `delete_section`, `insert_before_heading`, `insert_after_heading`.

Their `fields` are the edit anchor (`heading` + `content`, or `old` + `new`) plus `path` and `document_hash`. Unlike `norn edit` — which reads the body up front and stamps a whole-body `replace_body` — a plan edit op resolves **at apply time**: the applier re-reads the current body under the `document_hash` check and applies the edit through the same transform engine. This lets a section edit compose into one plan with other ops — e.g. a `set_frontmatter` (status change) and an `append_to_section` (history line) applied together. Multiple edit ops on the same document apply in plan order against the evolving body, sharing one `document_hash` precondition.

## Options

| Flag | Effect |
|---|---|
| `--dry-run` | Preview; check preconditions without writing. |
| `--yes` | Skip the TTY confirmation and apply. |
| `--out <PATH>` | Write the JSON `ApplyReport` to a file. |
| `--input-format json\|yaml` | Override plan-format detection (required for YAML on stdin). |

## Output formats

`--format records` (the default; human summary) or `json` (the full `ApplyReport` envelope: `schema_version`, `trace_id`, `plan_hash`, `vault_root`, `dry_run`, `applied`, `skipped`, `failed`, `remaining`, `operations`, `warnings`). The default is always `records` — there is no TTY/pipe auto-detection and no `paths` format. `--out` writes the JSON report independently of `--format`.

## See also

- [`repair`](repair.md) — produce the plan `migrate` applies.
- [`validate`](validate.md) — re-check the vault after applying.
- Run `norn migrate --help` for the full flag reference.
