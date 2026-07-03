---
title: get
description: Get one or more documents in full — frontmatter, headings, and links.
---

# norn get

Get one or more documents in detail — frontmatter, headings, and outgoing, incoming, and unresolved links. Where `find` selects a set by predicate, `get` selects named targets by identity. Each target resolves from a vault-relative path, a unique case-insensitive stem, or a wikilink-shaped string.

## Examples

```bash
norn get notes/my-note.md
# full record: frontmatter, headings, and links

norn get "My Note"
# resolve by stem (case-insensitive) instead of path

norn get notes/a.md notes/b.md --col title,status
# several documents, narrowed to two fields

norn get notes/my-note.md --col .incoming_links
# just the backlinks

norn get notes/my-note.md --all-cols --format json
# the complete structured dump, including body

norn get notes/my-note.md --format markdown
# rebuild the document as Markdown (frontmatter + body)
```

## Targets

Each `<DOC>` argument resolves from:

- a **vault-relative path** — `notes/my-note.md`
- a **document stem** — `my-note` or `My Note` (case-insensitive; must be unique)
- a **wikilink-shaped string** — `[[my-note]]`, with or without brackets; anchor (`#section`), block-ref (`^id`), and pipe-alias (`|label`) suffixes are stripped before resolution

An ambiguous target emits one record per resolved candidate. Multiple targets are accepted in one call.

## Selecting fields — `--col` and `--all-cols`

The `--col` vocabulary is identical to `norn find`:

- **Bare names select frontmatter fields:** `--col status,title`.
- **Structural facets are dot-prefixed:** `.path`, `.stem`, `.frontmatter`, `.headings`, `.outgoing_links`, `.unresolved_links`, `.incoming_links`, `.body`, `.raw`, `.document_hash`.
- **Default (no `--col`):** frontmatter, headings, and links. Body is included only with `--all-cols` or `--col .body`.
- **`--all-cols`:** every frontmatter field plus every cache-served facet, including `.body`. Excludes `.raw` and `.document_hash` (opt-in/identity-class, requested only by name). Mutually exclusive with `--col`.

`.body` is the parsed body from the cache; `.raw` is the file's exact bytes from disk. `.document_hash` is the blake3 hex of the full file content — the same value plan ops carry and `edit --expected-hash` / `vault.edit`'s `expected_hash` compare against, so `get --col .document_hash` is how you read a document's hash to guard a later edit.

## Sorting and paging

`get` accepts `--sort`, `--desc`, `--limit`, `--no-limit`, and `--starts-at` over the named targets. Unlike `find`, `get` returns **every** named target by default — no implicit limit.

## Output formats

| Format | Shape | Stable contract |
|---|---|---|
| `records` | Vertical key-value block per document. TTY default. | No. |
| `paths` | One vault-relative path per line. | Yes. |
| `json` | A JSON array of document records, one per resolved target. (Unlike `find`, `get` emits a bare array, not a `{ total, … }` wrapper.) | Yes, versioned. |
| `jsonl` | One JSON object per line. | Yes. |
| `markdown` | The document rebuilt as Markdown (frontmatter + body). One document at a time. | `get`-only. |

## See also

- [`find`](find.md) — select a set of documents by predicate, with the same `--col` vocabulary.
- [`set`](set.md) — update a document's frontmatter or body.
- [`validate`](validate.md) — check documents against configured rules.
- Run `norn get --help` for the full flag reference.
