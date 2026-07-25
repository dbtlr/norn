---
title: describe
description: Describe the vault — structure counts, the declared config with --schema, a contents-summary with --data.
---

# norn describe

Describe the vault: how many folders it spans, how many path and creatable rules it declares, where the inbox points — and, with `--schema`, every one of those rules in full. This is the orient-first command — run it before creating or mutating anything in an unfamiliar vault, so placement decisions come from the vault's own configuration instead of guesswork. Add `--data` for a contents-summary (totals, per-field value distributions, date bounds) over the same filter surface `find`/`count` share.

## Examples

```bash
norn describe
# structure at a glance: folder / path-rule / creatable-rule counts + inbox

norn describe --format json
# the same summary, machine-readable

norn describe --schema
# every folder, every rule with its frontmatter defaults, the full schema

norn describe --schema --format json
# the whole declared config as one object — the orient-first call for an agent

norn describe --data
# the summary, plus a contents-summary: totals and per-field distributions

norn describe --by type,status
# distribute two named fields explicitly; implies --data

norn describe --data --limit 5
# cap each field's shown value-buckets at 5 (default 20; 0 = no cap)

norn describe --data --eq type:note --format json
# contents-summary scoped to one filtered subset — same predicates as find/count
```

## Structure — counts by default, the declared config under `--schema`

Bare `describe` reports the structure as counts — how many `folders` hold documents, how many `path_rules` and `creatable_rules` are declared, and the `inbox` target:

```
folders    124
path rules 21
creatable  0
inbox      (none)
```

All four lines always print: in a counts block `0` is the answer, and an unconfigured inbox reads `(none)` rather than dropping its line. `--schema` expands that same structure into the config itself:

| Field | Meaning under `--schema` |
|---|---|
| `folders` | Distinct vault-relative directories that currently hold documents, sorted. The vault root is `""` (rendered `(root)` in `records`). |
| `path_rules` | Each configured rule with a `match.path` glob: the glob plus the `frontmatter_defaults` a document at a matching path inherits. |
| `creatable_rules` | Rules usable with `norn new --as <rule>`: `name`, the `target` path template, `required_vars` (from `{{var.X}}` tokens in the template), `frontmatter_defaults`, and an optional `body` scaffold. Only rules declaring both `name` and `target` are creatable. |
| `inbox` | The configured `inbox.path`, if set — where `norn new --title "…"` (no path, no `--as`) routes an unrouted create. `null` when unconfigured. |
| `schema` | The full `validate` config, serialized verbatim — every rule's `required_frontmatter`, `forbidden_frontmatter`, `field_types`, `allowed_values`, and path/frontmatter selectors. Nothing is summarized or dropped, so this is the authoritative source for "what does this vault require." |

`--schema` is where the declared-config dump lives today; it moves to [`config show`](config.md) — the verb that owns config data — when that command lands, and this flag retires with it.

Under `--schema`, `records` prints only the sections the vault declares something for (no rules, no `path rules` section; no `validate` config, no `schema` section) and renders rule defaults and the schema as YAML — the syntax `.norn/config.yaml` declares them in.

The two payloads fill `--format json` differently:

- **Default** — exactly four scalar keys (`folders`, `path_rules`, `creatable_rules`, `inbox`), always present, one per records count line; `inbox` is `null` when unconfigured (`(none)` in `records`). `data` is the only optional key: present when `--data`/`--stats`/`--by` was passed, absent otherwise.
- **`--schema`** — every report key, always present: the `folders` array, the `path_rules` / `creatable_rules` arrays (`[]` when the vault declares none), `inbox` (`null` when unconfigured), `schema`, and the same optional `data`. A section `records` omits still appears here as an empty array or object.

## Contents-summary — `--data`, `--stats`, `--by`, `--limit`

| Flag | Effect |
|---|---|
| `--data` | Add the contents-summary: total document count, per-field value distributions, and date-field bounds. |
| `--stats` | Alias for `--data`. Identical output. |
| `--by <FIELD1,FIELD2,...>` | Distribute exactly these fields, comma-separated, bypassing the automatic selection. Implies `--data`. |
| `--limit <N>` | Max value-buckets shown per field. Default 20; `0` means no cap. Only meaningful with `--data`/`--stats`/`--by`. |

Without `--by`, fields are auto-selected: every frontmatter key present across the matched documents, each checked against an identity-skip heuristic — a field whose distinct-value ratio is too high (near-unique per document, e.g. `title`) is dropped from the distribution and instead listed under `skipped` with its `distinct`/`total` counts, since a full breakdown of a near-unique field isn't a useful summary. `--by` bypasses that heuristic entirely: every named field is shown, even one that would otherwise be identity-skipped.

Fields declared `date` or `datetime` in any rule's `field_types` get a chronological `min`/`max` bound instead of a value distribution, shown under `dates` — unless `--by` names them explicitly, in which case they're distributed like any other field and no auto bounds are computed (avoiding rendering the same field twice). A date field whose values don't actually parse as valid ISO dates produces no bounds and falls back to a normal distribution.

Each returned field distribution carries a `more` count — the number of additional value-buckets beyond what `--limit` shows — so truncation is visible rather than silent.

## Filters

`--data` (and `--stats`/`--by`) scope the contents-summary to a filtered subset using the exact same predicates as [`find`](find.md) and [`count`](count.md), ANDed together: `--text`, `--eq`, `--not-eq`, `--in`, `--not-in`, `--starts-with`, `--ends-with`, `--contains`, `--has`, `--missing`, `--before`, `--after`, `--on`, `--path`, `--links-to`, `--unresolved-links`. See [`find`'s filter table](find.md#filters) for the full semantics of each. Filters have no effect on the structure fields (`folders`, `path_rules`, `creatable_rules`, `inbox`, `schema`) — those describe the vault's configuration, not its current document set, so they're unaffected by any filter.

## Output formats

| Format | Shape | Stable contract |
|---|---|---|
| `records` | Human-legible: structure counts (or, under `--schema`, the sectioned config), then (with `--data`) total, distributions, dates, and skipped fields. Default. | No — never parse it. |
| `json` | A single object carrying exactly what `records` carries. Default: `{ folders, path_rules, creatable_rules }` as integer counts, `inbox`, and (when requested) `data: { total, fields[], dates[], skipped[] }`. Under `--schema`: `folders` as the directory array, `path_rules` / `creatable_rules` as full rule objects, `inbox`, `schema`, and the same optional `data`. | Yes. |

`--format json` returns the JSON of whatever the invocation prints without it, so the flag never changes *which* payload you get — only its encoding. `describe` has no `paths`/`jsonl` format — it describes the vault as a whole, not a per-document row set.

`records` pages through `$PAGER` (default `less -FRX`) when stdout is a TTY and the output is longer than the terminal — `--schema` on a large vault is the common case. `--no-pager` streams it straight out; `json` never pages.

## See also

- [`vault.describe`](../mcp-server.md) — the MCP equivalent. It always returns the full declared-config report (the `--schema` payload); the counts projection is a CLI display choice and has no MCP parameter.
- [`new`](new.md) — `--as <rule>` consumes `creatable_rules`; the inbox fallback mode consumes `inbox`.
- [`config`](config.md) — `config show` for effective paths/counts; `describe --schema` for the declared rules as the engine resolves them; `describe --data` for document-content distributions.
- [`find`](find.md) / [`count`](count.md) — the same filter surface, returning matching documents or a grouped count instead of a vault-wide summary.
- Run `norn describe --help` for the full flag reference.
