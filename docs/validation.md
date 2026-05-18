---
title: Validation and repair
description: Finding codes, summary output, triage filters, the schema-versioned repair plan, and the apply contract.
---

# Validation and repair

`vault validate` is the detection surface. `vault repair plan` and `vault repair apply` are the planning and writing surfaces. Together they form the deterministic drift-healing loop: detect, plan, apply, verify.

## The validate command

`vault validate` is read-only. It runs the graph builder, applies configured `validate.rules`, and emits one finding per violation.

Findings are emitted as flat JSON objects keyed by `code`, with variant-specific fields present only when applicable. Use `--format jsonl` for one finding per line, `--format json` for an array, or `--format table` for human inspection.

```bash
vault validate --format jsonl
vault validate --code frontmatter-invalid-type --field created --format jsonl
vault validate --rule typed-note --path "notes/**/*.md" --format jsonl
```

## Finding codes

| Code | Severity | Source |
|---|---|---|
| `link-unresolved` | warning | Body or frontmatter link target not found. Carries `unresolved_reason`: `target-missing`, `anchor-missing`, `block-ref-missing`. |
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
vault validate --summary --format table
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

Comma-separated values within one filter are ORed (`--code link-unresolved,link-ambiguous`); different filters are ANDed.

```bash
vault validate --code link-unresolved --reason target-missing --format jsonl
vault validate --code frontmatter-disallowed-value --field status --summary --format json
vault validate --severity error --format jsonl
```

`--target` matches the raw parsed link target string — not a fuzzy stem, a resolved path, or a normalized candidate.

## Workflow recipes

### Size a queue, then read it

```bash
vault validate --summary --code frontmatter-invalid-type --field created --format table
vault validate --code frontmatter-invalid-type --field created --format jsonl
```

### Split link cleanup by failure mode

```bash
vault validate --code link-unresolved --reason target-missing --format jsonl
vault validate --code link-unresolved --reason anchor-missing,block-ref-missing --format jsonl
vault validate --code link-ambiguous --summary --format table
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
  "schema_version": 3,
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

Each planned change carries the target path, document hash precondition, finding context, operation, field, expected old value when available, and new value when applicable.

Skipped findings carry `skip_reason` (`unsupported`, `ambiguous`, `missing_hash`, `precondition_failed`), a free-form `reason`, candidates for ambiguous links, and suggested next actions. Fix the repairability problem, then rerun `repair plan`.

### Supported actions

The first supported repair actions are frontmatter-only:

- `set_frontmatter` — set a scalar field to a new value.
- `remove_frontmatter` — remove a field entirely.

Repair rule `match` supports `code`, `rule`, `field`, and `actual_value`. Matches are exact and type-sensitive. A rule must declare exactly one action.

## Repair apply

`vault repair apply <plan>` applies frontmatter-only plans. Apply writes by default because the command is explicit; pass `--dry-run` to preview.

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
