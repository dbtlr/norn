---
title: find
description: Find documents in the vault — full-text and metadata filters with sort, limit, and paging.
---

# norn find

Find documents in the vault by frontmatter, body text, path, or link relationship. `find` returns a set; pair it with `get` to inspect a single document in depth. A query needs at least one filter or `--all` — a bare `norn find` prints its help instead of dumping the whole vault.

## Examples

```bash
norn find --eq type:note --limit 5
# at most 5 documents with type: note

norn find --text "reorg" --format paths
# full-text body search; one vault-relative path per line

norn find --eq status:draft --col title,status
# narrow the fields shown to title and status

norn find --has aliases --col title,aliases
# documents that declare an aliases field

norn find --in type:note,log --sort modified --desc
# two types in one query, newest first

norn find --starts-with tags:release: --col title,tags
# namespace enumeration: every document carrying a release:* tag

norn find --eq type:task --links-to notes/my-note.md --format paths
# tasks that link to a document (by path; stem or [[wikilink]] also accepted)

norn find --unresolved-links --format paths
# documents with at least one broken link

norn find --all --all-cols --format jsonl
# whole-vault structured dump, one JSON object per line
```

## Filters

All filters are ANDed together. Within `--in` and `--not-in`, comma-separated values are ORed.

| Filter | Matches |
|---|---|
| `--text <NEEDLE>` | Case-insensitive body substring. An empty string is a no-op. |
| `--eq <FIELD:VALUE>` | Frontmatter `field` equals `value` (JSON-typed). |
| `--not-eq <FIELD:VALUE>` | Frontmatter `field` does not equal `value`. |
| `--in <FIELD:V1,V2,…>` | Frontmatter `field` is one of the listed values. |
| `--not-in <FIELD:V1,V2,…>` | Frontmatter `field` is none of the listed values. |
| `--starts-with <FIELD:VALUE>` | Frontmatter `field` (or any array element) starts with `VALUE`. Case-sensitive; `%`/`_` are literal. |
| `--ends-with <FIELD:VALUE>` | Frontmatter `field` (or any array element) ends with `VALUE`. |
| `--contains <FIELD:VALUE>` | Frontmatter `field` (or any array element) contains `VALUE` as a substring. Frontmatter-scoped — body substring search is `--text`. |
| `--has <FIELD>` | Frontmatter `field` is present and non-null. |
| `--missing <FIELD>` | Frontmatter `field` is absent or null. |
| `--before <FIELD:DATE>` | Date `field` is before `DATE` (ISO 8601). |
| `--after <FIELD:DATE>` | Date `field` is after `DATE`. |
| `--on <FIELD:DATE>` | Date `field` equals `DATE`. Accepts `today`. |
| `--path <GLOB>` | Vault-relative path glob. |
| `--links-to <TARGET>` | Documents whose outgoing links resolve to `TARGET` (path, stem, or `[[wikilink]]`). Repeatable; multiple targets are ANDed. Resolved-only — `TARGET` must resolve to an existing document. |
| `--unresolved-links` | Documents with at least one unresolved link. |
| `--all` | Return every document. The escape hatch when no predicate fits; a full-vault dump is almost always a mistake, so it requires opt-in. |

Every value-comparing predicate (`--eq`, `--not-eq`, `--in`, `--not-in`, `--starts-with`, `--ends-with`, `--contains`, `--before`, `--after`, `--on`) is array-aware — an array-valued field matches when any element matches (`--not-eq`/`--not-in` when no element does). String and date values collapse `[[wikilink]]` brackets on both sides, so `--starts-with depends_on:NRN-` matches a stored `"[[NRN-123]]"` and `--on created:2026-01-01` matches a daily-note link `"[[2026-01-01]]"`. Number and boolean values compare typed — `--eq n:5` matches a stored `5`, `5.0`, or an array containing either, but never the string `"5"`. The rule is symmetric: a string value only matches text-typed storage (plus structured values — objects and nested arrays — by their JSON text, the one way to `--eq` those shapes) — a queried `"5"` never matches a stored number `5`. The three string operators compare literal text: no bool/number coercion of the value, no whitespace trimming (a quoted `'title:done '` keeps its trailing space), and no wildcards or regex. Non-string stored values compare by their JSON text rendering under the string operators — booleans as `true`/`false`, numbers in canonical form (a stored `2.50` is the value `2.5`).

Every frontmatter predicate — equality, membership, presence, the string operators, and the date operators — answers from the derived `document_fields` index (see [cache internals](../cache.md)) instead of scanning `frontmatter_json` when every field it touches is declared `indexed` (see [`config`](config.md)). Filtering on a field that isn't indexed still works — it just scans — and past 1,000 documents `find`/`count` warn once on stderr (`scanned N documents on unindexed field(s) 'x'; declare indexed: true (or a bounded type) to accelerate`) rather than silently staying slow.

## Selecting fields — `--col` and `--all-cols`

By default `find` shows frontmatter only. `--col` narrows or extends that selection; the vocabulary is shared with `norn get`.

- **Bare names select frontmatter fields:** `--col status,title`.
- **Structural facets are dot-prefixed:** `.path`, `.stem`, `.frontmatter` (the whole block), `.headings`, `.outgoing_links`, `.unresolved_links`, `.incoming_links`, `.body`, `.document_hash`.
- **`--all-cols`** emits the full structured dump — whole frontmatter plus every cache-served facet except the opt-in `.stem` and `.document_hash`. Mutually exclusive with `--col`.

`.body` comes from the cache; `.document_hash` is the full-content blake3 hex (the value `edit --expected-hash` compares against). On `paths` format `--col` is ignored with a warning — paths output is a single path per line by definition.

## Sorting and paging

| Flag | Effect |
|---|---|
| `--sort <FIELD>` | Sort by a frontmatter key, `path`, or `stem`. Ascending by default. |
| `--desc` | Sort descending (only meaningful with `--sort`). |
| `--limit <N>` | Maximum records to return. `find` defaults to 10. |
| `--no-limit` | Return all records. Overrides `--limit`. |
| `--starts-at <N>` | 1-indexed paging offset. Default 1. |

## Output formats

`--format` auto-detects by destination: a TTY gets `records`, a pipe gets `paths`.

| Format | Shape | Stable contract |
|---|---|---|
| `records` | Human-legible key-value blocks, colored on a TTY. | No — never parse it. |
| `paths` | One vault-relative path per line. | Yes. |
| `json` | A single object: `{ total, returned, starts_at, documents[] }`. | Yes, versioned. |
| `jsonl` | One JSON object per line, no wrapper. | Yes. |

A `records` render longer than the terminal pages through `$PAGER` (default
`less -FRX`) on a real terminal. `--no-pager` writes straight to stdout
instead. Piped/non-terminal output is unaffected either way — the pager only
ever engages on a real terminal, and only for `records`.

## Recipes

```bash
# Feed matches into another tool
norn find --eq type:task --format paths | xargs -I{} norn get {}

# Count instead of list — same filter surface
norn count --eq type:note --by status

# Find broken links, then plan repairs
norn find --unresolved-links --format paths
```

## See also

- [`get`](get.md) — inspect a single document in full, with the same `--col` vocabulary.
- [`count`](count.md) — grouped or total counts over the same filters.
- [`validate`](validate.md) — find documents that violate configured rules.
- Run `norn find --help` for the full, always-current flag reference (`-h` for the compact version).
