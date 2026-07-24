---
title: Validation and repair
description: Finding codes, summary output, triage filters, the schema-versioned MigrationPlan, and the apply contract.
---

# Validation and repair

`norn validate` is the detection surface. `norn repair --plan` and `norn apply` are the planning and writing surfaces. Together they form the deterministic drift-healing loop: detect, plan, apply, verify.

## The validate command

`norn validate` is read-only. It runs the graph builder, applies configured `validate.rules`, and emits one finding per violation.

Findings are emitted as flat JSON objects keyed by `code`, with variant-specific fields present only when applicable. Use `--format jsonl` for one finding per line, `--format json` for a wrapped envelope (`{"total": N, "findings": [...]}`), or `--format records` for human-readable output on a TTY (the default).

```bash
norn validate --format jsonl
norn validate --code field-type-invalid --field created --format jsonl
norn validate --rule typed-note --path "notes/**/*.md" --format jsonl
```

## Finding codes

This is the complete, authoritative list of finding codes norn emits (also referenced from [`validate`](commands/validate.md#finding-codes)).

| Code | Severity | Source |
|---|---|---|
| `read-failed` | error | The document could not be read from disk. Carries `diagnostic`. |
| `frontmatter-unclosed` | warning | Frontmatter `---` opener has no closing `---`. Carries `diagnostic`. |
| `frontmatter-parse-failed` | warning | YAML frontmatter could not be parsed. Carries `diagnostic`. |
| `frontmatter-json-conversion-failed` | warning | Parsed YAML frontmatter could not be converted to JSON. Carries `diagnostic`. |
| `link-target-missing` | warning | Body or frontmatter link target not found in the vault. |
| `link-anchor-missing` | warning | Link target document exists, but the referenced heading anchor is not found. |
| `link-block-missing` | warning | Link target document exists, but the referenced block ID is not found. |
| `link-ambiguous` | warning | Stem lookup matched more than one document. Carries `candidates`. |
| `frontmatter-required-field-missing` | warning | `required_frontmatter` field is absent or null. Carries `field`, `rule`. |
| `frontmatter-forbidden-field` | warning | `forbidden_frontmatter` field is present. Carries `field`, `rule`. |
| `field-type-invalid` | warning | Present field doesn't match declared `field_types` shape. Carries `field`, `expected_type`, `rule`. |
| `frontmatter-exceeds-max-length` | warning | Present `string`/`list_of_strings` field matches its type's shape but exceeds the effective `max_length` bound. Carries `field`, `max_length`, `actual_length`, `rule`. |
| `value-not-allowed` | warning | Present scalar field value isn't in `allowed_values`. Carries `field`, `actual_value`, `allowed_values`, `rule`. |
| `document-misrouted` | warning | Document path matches no `allowed_paths` glob. Carries `allowed_paths`, `rule`. |
| `frontmatter-reference-type` | warning | A frontmatter wikilink resolves to a document whose `type` is outside the field's `field_references.target_type` set. Carries `field`, `reference`, `target`, `actual_type`, `allowed_types`, `rule`. |

For the selector + constraint model that produces these codes, see [rule-shape.md](rule-shape.md).

## Summary output

`norn validate --summary` emits grouped finding counts instead of raw findings. The schema includes:

- `total` — total finding count.
- `codes` — count per finding code.
- `severities` — count per severity.
- `rules` — count per rule name.
- `fields` — count per frontmatter field.
- `disallowed_values` — count per `(field, value)` pair.
- `invalid_types` — count per `(field, expected_type)` pair.
- `paths` — count per top-level path prefix.

Use summaries to size a cleanup queue before reading raw findings.

```bash
norn validate --summary --format records
norn validate --summary --code field-type-invalid --field created --format json
```

## Triage filters

`norn validate` supports filter flags that apply to both raw output and `--summary`:

| Filter | Matches |
|---|---|
| `--code` | Finding code. |
| `--severity` | `warning` or `error`. |
| `--field` | Frontmatter field name (for findings that carry one). |
| `--rule` | Rule name (for findings produced by a scoped rule). |
| `--path` | Vault-relative path glob (path-segment semantics). |
| `--target` | Raw parsed link target string (exact match). |
| `--reason` | Unresolved-link reason: `target-missing`, `anchor-missing`, `block-ref-missing`, `ambiguous`. |

Comma-separated values within one filter are ORed (`--code link-target-missing,link-ambiguous`); different filters are ANDed. Glob patterns also work within `--code` (`--code 'link-*'` matches all four link codes).

```bash
norn validate --code link-target-missing --format jsonl
norn validate --code value-not-allowed --field status --summary --format json
norn validate --severity error --format jsonl
```

`--target` matches the raw parsed link target string — not a fuzzy stem, a resolved path, or a normalized candidate.

## Workflow recipes

### Size a queue, then read it

```bash
norn validate --summary --code field-type-invalid --field created --format records
norn validate --code field-type-invalid --field created --format jsonl
```

### Split link cleanup by failure mode

```bash
norn validate --code link-target-missing --format jsonl
norn validate --code link-anchor-missing,link-block-missing --format jsonl
norn validate --code link-ambiguous --summary --format records
norn validate --code 'link-*' --format jsonl
```

### Scope by path

```bash
norn validate --path "notes/**/*.md" --summary --format json
norn validate --path "tasks/**/*.md" --rule task-status --format jsonl
```

## Repair planning

`norn repair --plan` runs validation, applies the same triage filters, and converts findings matched by configured `repair.rules` into an explicit JSON `MigrationPlan`.

```bash
norn repair --plan --format json
norn repair --plan --out plan.json
norn repair --plan --code value-not-allowed --field status --out plan.json
```

### Plan schema

```json
{
  "schema_version": 2,
  "vault_root": "/abs/path/to/vault",
  "preconditions": [
    {
      "id": "note-owner",
      "kind": "owner_set",
      "selector": { "stem": "x" },
      "expected_paths": ["notes/x.md"]
    }
  ],
  "operations": [ { "kind": "set_frontmatter", "fields": { "...": "..." } } ],
  "skipped": [ { "finding_code": "value-not-allowed", "path": "notes/x.md", "reason": "no-rule-matched" } ]
}
```

Each planned operation carries a `kind` and a `fields` object with the target path, document-hash precondition, and the operation's data (field/value, destination, etc.).

The optional `preconditions` array protects logical identities rather than physical paths. An `owner_set` entry asserts the exact current path set selected by `{ "stem": "…" }`, `{ "eq": ["field:value", "…"] }`, or `{ "stem_from_operation": "operation-id" }`. The operation-derived form resolves `{{seq}}` first. Norn evaluates the whole array once under the mutation lock; any mismatch refuses before an operation runs.

Each skipped finding carries `finding_code` (the underlying validate code, e.g. `link-ambiguous`), `path`, and a single canonical `reason` — the stable **kebab-case** skip-reason code, one of `missing-default`, `link-decision-needed`, `no-rule-matched`, `graph-diagnostic`, `ambiguous-target`, `missing-hash`, or `precondition-failed`. Branch on `reason`; the summary's `by_reason` map keys on the same codes. Fix the repairability problem, then rerun `repair --plan`.

### Supported actions

The supported repair actions are:

- `set_frontmatter` — replace an existing scalar field's value.
- `remove_frontmatter` — remove a field entirely.
- `add_frontmatter` — insert a missing scalar field.
- `move_document` — move or rename a file, with automatic backlink rewriting on apply.
- `rewrite_link` — rewrite a broken wikilink in the source document to a new target. Proposed automatically by the closest-match algorithm for `link-target-missing` findings; preserves display text, anchor, and block-ref suffixes.
- `create_document` — create a brand-new document with synthesized frontmatter and body. Emitted exclusively by `norn new`; not config-rule-triggerable.

Repair rule `match` supports `code`, `rule`, `field`, and `actual_value`. Matches are exact and type-sensitive. A rule must declare exactly one action (for configurable rules; `rewrite_link` is emitted by the closest-match planner, not from config rules).

> **Note on emitter-only ops:** Two plan op variants are emitter-only — `replace_body` (emitted by `norn set --body-from-stdin`) and `create_document` (emitted by `norn new`). Neither is config-rule-triggerable.

## Repairable findings

The validate → plan → apply → verify loop closes for these finding classes when a matching repair rule is authored:

| Finding code | Repair action | Notes |
|---|---|---|
| `value-not-allowed` | `set_frontmatter` | Replace the disallowed value with a configured value. |
| `frontmatter-required-field-missing` | `add_frontmatter` | Insert the missing field with a configured value. |
| `frontmatter-forbidden-field` | `remove_frontmatter` | Remove the forbidden field. |
| `document-misrouted` | `move_document` | Move the file to a configured destination (with backlink rewriting). |
| `link-target-missing` | `rewrite_link` | Closest-match rewrite proposed automatically. Use `--confidence high` to keep only slug-normalized-identity matches. |

Findings without a matching deterministic rule are reported as skipped fallout in the MigrationPlan's `skipped[]` with `reason: "no-rule-matched"`.

## Apply

`norn apply [<plan>]` applies `MigrationPlan` artifacts. Apply writes by default; pass `--dry-run` to preview.

The positional is optional: omit it (or pass `-`) to read the plan from stdin. The pipeline form composes plan generation and apply in one shot:

```bash
norn apply plan.json --dry-run
norn repair --plan --format json | norn apply - --dry-run
norn apply plan.json
norn apply plan.json --out report.json
```

Output formats: `--format records` (the default; human summary) or `--format json` (the full `ApplyReport` envelope). `--out <PATH>` writes the JSON report to file independently of `--format`. There is no `--format paths` for `apply` and no TTY/pipe auto-detection — `records` is always the default unless overridden.

Apply rejects:

- Unsupported plan schema versions.
- Plans for a different vault root than the current invocation.
- Stale document hashes (the document changed since the plan was created).
- Conflicting field changes within one apply run.
- Expected-old-value mismatches.
- Logical owner-set mismatches or conflicting create claims.

First-class owner-set preconditions form one atomic pre-write barrier. A failure returns `outcome: "refused"` (exit 2), records the failed precondition with sorted expected/actual paths and a stable code, marks every operation `not-run`, and leaves the vault byte-identical. Later operation-class failures retain the existing truthful partial-apply contract described in [Error and outcome contract](errors.md).

### Vaults are self-contained

Every mutation target must resolve inside the vault root. A vault is self-contained: `norn` refuses — as a preflight, before writing a single byte — any create, move, delete, or edit whose target is an absolute path, contains a `..` parent-traversal component, or reaches outside the vault through a directory symlinked out of it. Outbound symlinks are unsupported; a directory inside the vault that points elsewhere is not a valid mutation target. The refusal names the offending path and exits with the preflight code (2). This guarantee holds for `norn new` as well as plan apply — the same containment gate backs both.

Frontmatter apply preserves Markdown body content byte-for-byte. YAML lines untouched by a repair are preserved exactly (comments, quote style, key ordering). YAML lines touched by a repair preserve the original quote style when the new value is representable in that style; otherwise apply upgrades to the minimum sufficient style and never downgrades.

A `set_frontmatter` change targeting a block-style value (block sequence, block mapping, block literal, block folded, or flow sequence/mapping) returns `cannot minimal-edit` rather than silently rewriting the structure.

When a plan contains `move_document` changes, apply writes to multiple files: the moved file itself plus every backlinking file that contains a rewritable link. The `move_document` operation's `cascade` object (`planned`, `applied`, `skipped`, `failed`, `files`) summarizes everything the cascade touched; pass `--verbose` for the per-link `rewrites`/`skips` detail.

### Apply report

The JSON `ApplyReport` (`--format json`, or `--out <PATH>`) carries top-level counts plus a per-operation list — there is no separate plan-context envelope:

```json
{
  "schema_version": 3,
  "trace_id": "…",
  "plan_hash": "…",
  "vault_root": "/abs/path/to/vault",
  "dry_run": false,
  "applied": 3,
  "skipped": 1,
  "failed": 0,
  "remaining": 0,
  "preconditions": [
    { "id": "note-owner", "status": "passed", "expected_paths": ["notes/x.md"], "actual_paths": ["notes/x.md"] }
  ],
  "operations": [ { "op_id": "…", "kind": "set_frontmatter", "status": "applied", "summary": "…" } ],
  "warnings": []
}
```

`skipped`/`failed`/`remaining` are apply-time counts (operations the batch didn't apply because an earlier precondition aborted the run, or that failed outright) — they are not the plan's roster of findings that never became operations. To see why a finding never made it into the plan at all, read the `MigrationPlan`'s own `skipped` list, not the apply report.

## Stable repair loop

```bash
norn validate --summary --format json
norn repair --plan --out plan.json
norn apply plan.json --dry-run --format json
norn apply plan.json --format json --yes
norn validate --summary --format json
```

For live maintenance with a snapshot tag:

```bash
git status --short
git tag snapshot/vault-repair-$(date +%Y%m%d-%H%M%S)
norn repair --plan --out plan.json
norn apply plan.json --dry-run --format json
norn apply plan.json --format json --yes
git diff --check
git diff
norn validate --summary --format json
```

See [examples/repair-recipe.sh](../examples/repair-recipe.sh) for a runnable version.

## Link and path planning

To surface link drift across the norn before moving or deleting documents, use `norn validate --code 'link-*'`. This returns unresolved links, ambiguous links with candidate paths, and related link findings in the standard validation shape.

```bash
norn validate --code 'link-*' --format jsonl
norn validate --code 'link-*' --target "notes/some-note.md" --format jsonl
norn validate --code 'link-*' --summary --format json
```

To preview the effect of moving a document (backlink rewrites, stem collisions, affected files), use `norn move` with `--dry-run`:

```bash
norn move Inbox/task.md Projects/demo/task.md --dry-run
norn move Inbox/task.md Projects/demo/task.md --dry-run --format json
```

To preview deletion risk (incoming links that would break), use `norn delete` with `--dry-run`:

```bash
norn delete notes/old-note.md --dry-run
norn delete notes/old-note.md --dry-run --format json
```

These dry-run passes separate deterministic facts (exact backlinks, path conflicts) from ambiguous/skipped fallout, without writing to the vault.

## See also

- [Validate rule shape](rule-shape.md) — selectors + constraints conceptual model.
- [Configuration](configuration.md) — `validate.rules` and `repair.rules` schema.
- [Agent workflows](agent-workflows.md) — stable contracts and agent loop patterns.
