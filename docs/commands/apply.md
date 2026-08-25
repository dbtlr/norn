---
title: apply
description: Apply a MigrationPlan — execute move, delete, rewrite, and frontmatter ops from a plan file.
---

# norn apply

Apply a `MigrationPlan`, either from `norn repair --plan` or from a hand-authored file. `apply` checks plan-level preconditions before any operation runs. It checks operation preconditions before each operation class writes. A later operation failure can leave an earlier operation applied. `apply` is the only command that writes from a plan.

## Examples

```bash
norn apply plan.json --dry-run
# walk the plan and check preconditions; no write

norn apply plan.json --yes
# apply the plan

norn repair --plan --format json | norn apply - --yes
# generate and apply in one pipeline (- reads the plan from stdin)

norn apply plan.json --out report.json
# write the JSON ApplyReport to a file
```

## How apply writes

`apply` walks the plan in order:

1. Load the plan and verify its schema version.
2. Confirm the plan's recorded vault root matches the current vault.
3. Resolve every `create_document` path, including `{{seq}}` templates.
4. Evaluate all first-class owner-set preconditions once, under the mutation lock — abort before any operation if one fails.
5. Re-read each source document and verify its hash and `expected_old_value` checks as its operation class runs.
6. Write the changes, preserving each document's Markdown body.

`--dry-run` walks steps 1–5 without writing — it performs the hash and `expected_old_value` re-check (step 5), stopping short only of the writes in step 6. Dry-run and a real apply evaluate the same preconditions the same way, so a plan a dry-run refuses a real apply refuses identically. An **owner-set precondition mismatch** is evaluated plan-level, before any operation runs, and always refuses cleanly — `outcome: refused`, **exit 2**, the vault left byte-identical — rendering the full `ApplyReport` (the failed precondition, every operation `not-run`) and honoring `--out`. The **per-operation preconditions** — a stale `document_hash`, an `expected_old_value` mismatch, a failed edit anchor — are verified per operation class, before that class writes; the section/body edit ops go further — every target's hash and transform is validated before any edit is written, so the edit batch as a whole either applies or aborts. When such a failure is caught before anything in the plan has written, the apply refuses identically — `outcome: refused`, exit 2, the vault byte-identical — surfacing the coded `{ code, message, path }` error (on stdout under `--format json`, as an `error:` line on stderr otherwise) and producing no `ApplyReport`, so `--out` writes nothing. When an earlier operation class in the same plan has already written, the failure is a **partial apply**, not a refusal — `outcome: failed`, exit 1, the vault partially mutated (see the [error and outcome contract](../errors.md)). Distinct from both is a **malformed plan** — an unparseable plan body, or an unsupported `schema_version` — which is rejected as a bare error on stderr before the plan is evaluated, with no report. To re-check the vault after applying, run [`norn validate`](validate.md) as a follow-up.

## Plan input

The required `<PLAN>` argument is a path to a JSON or YAML plan. Use `-` to read from stdin. The input format follows the file extension. For a YAML plan on stdin, pass `--input-format yaml`.

## Plan operations

Each plan operation has a `kind` and a `fields` object. Structural operations are `move_document`, `move_folder`, `delete_document`, `rewrite_link`, `rewrite_wikilink`, `create_document`, `replace_body`, `set_frontmatter`, `add_frontmatter`, and `remove_frontmatter`. A plan can also carry the body-edit operations from [`norn edit`](edit.md): `str_replace`, `replace_section`, `append_to_section`, `delete_section`, `insert_before_heading`, and `insert_after_heading`.

Their `fields` are the edit anchor (`heading` + `content`, or `old` + `new`) plus `path` and `document_hash`. Unlike `norn edit` — which reads the body up front and stamps a whole-body `replace_body` — a plan edit op resolves **at apply time**: the applier re-reads the current body under the `document_hash` check and applies the edit through the same transform engine. This lets a section edit compose into one plan with other ops — e.g. a `set_frontmatter` (status change) and an `append_to_section` (history line) applied together. Multiple edit ops on the same document apply in plan order against the evolving body, sharing one `document_hash` precondition.

## Logical owner preconditions

`MigrationPlan` schema v2 can carry an optional top-level `preconditions` array. Each entry has `kind: "owner_set"`, a unique `id`, a selector, and the exact sorted vault-relative paths expected to own that logical identity:

```json
{
  "id": "project-owner",
  "kind": "owner_set",
  "selector": { "eq": ["type:project", "key:MMR"] },
  "expected_paths": ["projects/mimir.md"]
}
```

The deliberately narrow selectors are `{ "stem": "MMR-42" }`, `{ "eq": ["type:project", "key:MMR"] }`, and `{ "stem_from_operation": "create-task" }`. The last form references the unique `id` of a `create_document` operation and checks the stem of its resolved path. A mismatch refuses the whole plan before any operation runs; every operation is reported `not-run`.

## Options

| Flag | Effect |
|---|---|
| `--dry-run` | Preview; check preconditions without writing. |
| `--yes` | Skip the TTY confirmation and apply. |
| `--out <PATH>` | Write the JSON `ApplyReport` to a file. |
| `--input-format json\|yaml` | Override plan-format detection (required for YAML on stdin). |

## Output formats

`--format records` (the default; human summary) or `json` (the full `ApplyReport` v3 envelope: `schema_version`, `trace_id`, `plan_hash`, `vault_root`, `dry_run`, `applied`, `skipped`, `failed`, `remaining`, `preconditions`, `operations`, `warnings`). The default is always `records` — there is no TTY/pipe auto-detection and no `paths` format. `--out` writes the JSON report independently of `--format`.

## See also

- [`repair`](repair.md) — produce the plan `apply` applies.
- [`validate`](validate.md) — re-check the vault after applying.
- Run `norn apply --help` for the full flag reference.
