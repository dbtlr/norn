---
title: count
description: Count documents in the vault — grouped or total — over the find filter surface.
---

# norn count

Count documents in the vault, total or grouped by a frontmatter field. `count` shares the entire filter surface with `norn find` — every predicate that narrows a `find` query narrows a `count`.

## Examples

```bash
norn count
# total document count in the vault

norn count --eq type:note --by status
# notes only, grouped by status

norn count --path 'notes/**/*.md' --by type
# documents under notes/, grouped by type

norn count --by project,lifecycle --format json
# every project's lifecycle distribution in one call

norn count --eq type:task --format json
# machine-readable total
```

## Grouping

`--by <FIELD1,FIELD2,...>` groups the count by one or more frontmatter fields. One field emits a flat tally per distinct value. Several fields nest in order --- `--by project,lifecycle` yields one block per project, each holding that project's lifecycle distribution, with counts at the leaves. Documents missing a field bucket under `(missing)` at that level. Without `--by`, `count` emits only the total.

JSON shape: with one field, `by` is the field name (a string) and `groups` a flat value-to-count map --- this exact shape is stable for existing consumers. With several fields, `by` is the key list (an array) and `groups` nests one map level per key.

## Filters

`count` accepts the same predicates as `find`, ANDed together: `--text`, `--eq`, `--not-eq`, `--in`, `--not-in`, `--starts-with`, `--ends-with`, `--contains`, `--has`, `--missing`, `--before`, `--after`, `--on`, `--path`, `--links-to`, `--unresolved-links`. See [`find`](find.md) for the full table.

## Output formats

| Format | Shape | Stable contract |
|---|---|---|
| `text` | Records-block: the total, plus one tally row per group with `--by`. TTY default. | No. |
| `json` | Structured total and groups. | Yes, versioned. |

`count` returns a scalar or a small grouping, so it offers `text` and `json` only — not the document-oriented `paths` / `jsonl`.

## See also

- [`find`](find.md) — the same filters, returning the matching documents.
- Run `norn count --help` for the full flag reference.
