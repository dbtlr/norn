---
title: Validation and repair
description: Finding codes, summary output, triage filters, the schema-versioned repair plan, and the apply contract.
---

# Validation and repair

`vault validate` is the detection surface. `vault repair plan` and `vault repair apply` are the planning and writing surfaces. Together they form the deterministic drift-healing loop: detect, plan, apply, verify.

## The validate command

`vault validate` is read-only. It runs the graph builder, applies configured `validate.rules`, and emits one finding per violation.

Findings are emitted as flat JSON objects keyed by `code`, with variant-specific fields present only when applicable. Use `--format jsonl` for one finding per line, `--format json` for a wrapped envelope (`{"total": N, "findings": [...]}`), or `--format records` for human-readable output on a TTY (the default).

```bash
vault validate --format jsonl
vault validate --code frontmatter-invalid-type --field created --format jsonl
vault validate --rule typed-note --path "notes/**/*.md" --format jsonl
```

## Finding codes

| Code | Severity | Source |
|---|---|---|
| `link-target-missing` | warning | Body or frontmatter link target not found in the vault. |
| `link-anchor-missing` | warning | Link target document exists, but the referenced heading anchor is not found. |
| `link-block-missing` | warning | Link target document exists, but the referenced block ID is not found. |
| `link-ambiguous` | warning | Stem lookup matched more than one document. Carries `candidates`. |
| `frontmatter-parse-failed` | error | YAML frontmatter could not be parsed. Carries `diagnostic`. |
| `frontmatter-unclosed` | error | Frontmatter `---` opener with no closing `---`. |
| `frontmatter-required-field-missing` | warning | `required_frontmatter` field is absent or null. Carries `field`, `rule`. |
| `frontmatter-forbidden-field` | warning | `forbidden_frontmatter` field is present. Carries `field`, `rule`. |
| `frontmatter-invalid-type` | warning | Present field doesn't match declared `field_types` shape. Carries `field`, `expected_type`, `rule`. |
| `frontmatter-disallowed-value` | warning | Present scalar field value isn't in `allowed_values`. Carries `field`, `actual_value`, `allowed_values`, `rule`. |
| `document-misrouted` | warning | Document path matches no `allowed_paths` glob. Carries `allowed_paths`, `rule`. |

For the selector + constraint model that produces these codes, see [rule-shape.md](rule-shape.md).

## Summary output

`vault validate --summary` emits grouped finding counts instead of raw findings. The schema includes:

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
vault validate --summary --format records
vault validate --summary --code frontmatter-invalid-type --field created --format json
```

## Triage filters

`vault validate` supports filter flags that apply to both raw output and `--summary`:

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
vault validate --code link-target-missing --format jsonl
vault validate --code frontmatter-disallowed-value --field status --summary --format json
vault validate --severity error --format jsonl
```

`--target` matches the raw parsed link target string — not a fuzzy stem, a resolved path, or a normalized candidate.

## Workflow recipes

### Size a queue, then read it

```bash
vault validate --summary --code frontmatter-invalid-type --field created --format records
vault validate --code frontmatter-invalid-type --field created --format jsonl
```

### Split link cleanup by failure mode

```bash
vault validate --code link-target-missing --format jsonl
vault validate --code link-anchor-missing,link-block-missing --format jsonl
vault validate --code link-ambiguous --summary --format records
vault validate --code 'link-*' --format jsonl
```

### Scope by path

```bash
vault validate --path "notes/**/*.md" --summary --format json
vault validate --path "tasks/**/*.md" --rule task-status --format jsonl
```

## Repair planning

`vault repair plan` runs validation, applies the same triage filters, and converts findings matched by configured `repair.rules` into an explicit JSON repair plan.

```bash
vault repair plan --format json
vault repair plan --out repair.json
vault repair plan --code frontmatter-disallowed-value --field status --out repair.json
```

### Plan schema

```json
{
  "schema_version": 5,
  "vault_root": "/abs/path/to/vault",
  "source_filters": { "...": "..." },
  "summary": {
    "findings": 42,
    "planned_changes": 18,
    "skipped": {
      "unsupported": 20,
      "ambiguous": 3,
      "missing_hash": 0,
      "precondition_failed": 1,
      "total": 24
    }
  },
  "changes": [ { "...": "..." } ],
  "skipped_findings": [ { "...": "...", "skip_reason": "unsupported" } ]
}
```

Each planned change carries the target path, document hash precondition, finding context, operation, optional field (omitted for `move_document` changes), expected old value when available, new value when applicable, and — for moves — `destination`, `link_risk`, and any `warnings`.

Skipped findings carry `skip_reason` (`unsupported`, `ambiguous`, `missing_hash`, `precondition_failed`), a free-form `reason`, candidates for ambiguous links, and suggested next actions. Fix the repairability problem, then rerun `repair plan`.

### Supported actions

The supported repair actions are:

- `set_frontmatter` — replace an existing scalar field's value.
- `remove_frontmatter` — remove a field entirely.
- `add_frontmatter` — insert a missing scalar field.
- `move_document` — move or rename a file, with automatic backlink rewriting on apply.
- `rewrite_link` — rewrite a broken wikilink in the source document to a new target. Proposed automatically by the closest-match algorithm for `link-target-missing` findings; preserves display text, anchor, and block-ref suffixes.

Repair rule `match` supports `code`, `rule`, `field`, and `actual_value`. Matches are exact and type-sensitive. A rule must declare exactly one action (for configurable rules; `rewrite_link` is emitted by the closest-match planner, not from config rules).

## Repairable findings

The validate → plan → apply → verify loop closes for these finding classes when a matching repair rule is authored:

| Finding code | Repair action | Notes |
|---|---|---|
| `frontmatter-disallowed-value` | `set_frontmatter` | Replace the disallowed value with a configured value. |
| `frontmatter-required-field-missing` | `add_frontmatter` | Insert the missing field with a configured value. |
| `frontmatter-forbidden-field` | `remove_frontmatter` | Remove the forbidden field. |
| `document-misrouted` | `move_document` | Move the file to a configured destination (with backlink rewriting). |
| `link-target-missing` | `rewrite_link` | Closest-match rewrite proposed automatically. Use `--confidence high` to keep only slug-normalized-identity matches. |

Findings without a matching deterministic rule are reported as skipped fallout in the repair plan with `skip_reason: unsupported`.

## Repair apply

`vault repair apply <plan>` applies repair plans. Apply writes by default because the command is explicit; pass `--dry-run` to preview.

```bash
vault repair apply repair.json --dry-run --format json
vault repair apply repair.json --verify --format json
```

Apply rejects:

- Unsupported plan schema versions.
- Plans for a different vault root than the current invocation.
- Stale document hashes (the document changed since the plan was created).
- Conflicting field changes within one apply run.
- Expected-old-value mismatches.

Frontmatter apply preserves Markdown body content byte-for-byte. YAML lines untouched by a repair are preserved exactly (comments, quote style, key ordering). YAML lines touched by a repair preserve the original quote style when the new value is representable in that style; otherwise apply upgrades to the minimum sufficient style and never downgrades.

A `set_frontmatter` change targeting a block-style value (block sequence, block mapping, block literal, block folded, or flow sequence/mapping) returns `cannot minimal-edit` rather than silently rewriting the structure.

When a plan contains `move_document` changes, apply writes to multiple files: the moved file itself plus every backlinking file that contains a rewritable link. The apply output's `moved_files` and `rewritten_links` enumerate everything that was touched.

### Apply report

Apply output includes `plan_context` so broad plans remain explainable after applying deterministic changes:

```json
{
  "plan_context": {
    "skipped": {
      "unsupported": 1,
      "ambiguous": 0,
      "missing_hash": 0,
      "precondition_failed": 0,
      "total": 1
    }
  }
}
```

## Stable repair loop

```bash
vault validate --summary --format json
vault repair plan --out repair.json
vault repair apply repair.json --dry-run --format json
vault repair apply repair.json --verify --format json
```

For live maintenance with a snapshot tag:

```bash
git status --short
git tag snapshot/vault-repair-$(date +%Y%m%d-%H%M%S)
vault repair plan --out repair.json
vault repair apply repair.json --dry-run --format json
vault repair apply repair.json --verify --format json
git diff --check
git diff
```

See [examples/repair-recipe.sh](../examples/repair-recipe.sh) for a runnable version.

## Link and path planning

`vault repair links` is a read-only planning surface for link drift and path move/delete risk. It does not rewrite links or move files.

```bash
vault repair links --format json
vault repair links --target "notes/some-note.md" --format json
vault repair links --target "some-note" --format table
```

The report includes unresolved links, ambiguous links with candidate paths, path-style Markdown links worth reviewing before path moves, duplicate-stem risks for stem-style wikilinks, affected files, and optional move/delete risk for the selected `--target`.

Link and path planning separates deterministic facts from ambiguous/skipped fallout. It does not automatically resolve ambiguous links, guess missing semantic targets, or apply path rewrites.

## See also

- [Validate rule shape](rule-shape.md) — selectors + constraints conceptual model.
- [Configuration](configuration.md) — `validate.rules` and `repair.rules` schema.
- [Agent workflows](agent-workflows.md) — stable contracts and agent loop patterns.
