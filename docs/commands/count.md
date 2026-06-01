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

norn count --eq type:task --format json
# machine-readable total
```

## Grouping

`--by <FIELD>` groups the count by a frontmatter field, emitting one tally per distinct value. Without `--by`, `count` emits only the total.

## Filters

`count` accepts the same predicates as `find`, ANDed together: `--text`, `--eq`, `--not-eq`, `--in`, `--not-in`, `--has`, `--missing`, `--before`, `--after`, `--on`, `--path`, `--links-to`, `--unresolved-links`. See [`find`](find.md) for the full table.

## Output formats

| Format | Shape | Stable contract |
|---|---|---|
| `text` | Records-block: the total, plus one tally row per group with `--by`. TTY default. | No. |
| `json` | Structured total and groups. | Yes, versioned. |

`count` returns a scalar or a small grouping, so it offers `text` and `json` only — not the document-oriented `paths` / `jsonl`.

## See also

- [`find`](find.md) — the same filters, returning the matching documents.
- Run `norn count --help` for the full flag reference.
