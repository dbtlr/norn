---
title: repair
description: Surface deterministic-repair findings; --plan emits an inspectable MigrationPlan.
---

# norn repair

Turn validation findings into an inspectable, deterministic repair plan. Bare `norn repair` prints a findings summary; `norn repair --plan` emits a `MigrationPlan` describing every change it would make. Planning never writes — `norn apply` applies the plan.

## Examples

```bash
norn repair
# summarize repairable findings; no writes

norn repair --plan --out plan.json
# write a MigrationPlan to a file

norn repair --plan --format json | norn apply -
# generate a plan and apply it in one pipeline

norn repair --plan --format paths
# affected paths only; pipe to xargs

norn repair --plan --skip-reason ambiguous-target
# show only the ambiguous-target skips

norn repair --plan --severity error
# plan only error-level findings
```

## The plan/apply boundary

Repair runs in two halves. `norn repair --plan` reads validate findings and emits a `MigrationPlan` JSON artifact; planning never touches vault documents. [`norn apply`](apply.md) consumes that artifact and writes the changes, checking preconditions before any file is touched.

Each supported finding becomes a `PlannedChange` recording the path, field, new value, and the source document's hash at plan time — so apply can refuse to write if the file changed since planning. Re-run `--plan` after editing files between plan and apply.

Skipped findings carry a stable reason code: `missing-default`, `link-decision-needed`, `no-rule-matched`, `alias-shadowed`, `graph-diagnostic`, `ambiguous-target`, `missing-hash`, `precondition-failed`. Filter them with `--skip-reason <PATTERN>` (globs accepted, repeatable).

## Options

| Flag | Effect |
|---|---|
| `--plan` | Generate a `MigrationPlan` (read-only). Without it, `repair` prints a findings summary. |
| `--out <PATH>` | Write the JSON plan to a file instead of stdout (`--plan` only). |
| `--confidence high` | Drop medium-confidence closest-match proposals (and their footnotes). |
| `--skip-reason <PATTERN>` | Filter the skipped-findings list by reason code. |

`repair` also accepts the full `validate` triage filter set (`--code`, `--severity`, `--field`, `--rule`, `--path`, `--target`, `--reason`); a filter that excludes a finding from `validate` excludes it from the plan too.

## Output formats

With `--plan`: `report` (human summary, TTY default), `json` (the full `MigrationPlan` envelope — the only format `apply` consumes; pipe default), and `paths` (one affected path per line, deduplicated).

## See also

- [`validate`](validate.md) — the findings `repair` plans from.
- [`apply`](apply.md) — apply the plan.
- [Validation and repair](../validation.md) — closest-match rewrites, confidence bands, and the footnotes layer.
- Run `norn repair --help` for the full flag reference.
