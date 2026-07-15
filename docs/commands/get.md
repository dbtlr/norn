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
# print the exact source file, without newline normalization

norn get notes/my-note.md --section "Task Description" --section "Annotations" --format json
# just those two named sections' content (repeat --section per heading)
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
- **Structural facets are dot-prefixed:** `.path`, `.stem`, `.frontmatter`, `.headings`, `.outgoing_links`, `.unresolved_links`, `.incoming_links`, `.body`, `.document_hash`.
- **Default (no `--col`):** frontmatter, headings, and links. Body is included only with `--all-cols` or `--col .body`.
- **`--all-cols`:** every frontmatter field plus every cache-served facet, including `.body`, except the opt-in `.stem` and `.document_hash`. Mutually exclusive with `--col`.

`.body` is the parsed body from the cache. `.document_hash` is the blake3 hex of the full file content — the same value plan ops carry and `edit --expected-hash` / `vault.edit`'s `expected_hash` compare against, so `get --col .document_hash` is how you read a document's hash to guard a later edit.

## Selecting sections — `--section`

`--section "Heading"` reads a named section of the body instead of (or alongside) `--col`/`--all-cols` — it's a distinct flag, not a `--col` facet, so combine it freely. It is **repeatable**: pass it once per section (`--section "Task Description" --section "Annotations"`). Each occurrence is one whole heading string, so a heading that itself contains a comma (`--section "Risks, Open Questions"`) is addressable verbatim — the same way `edit` takes a heading as one whole string. Each heading is resolved with the exact same boundary and failure semantics as `edit --append-to-section` / `--replace-section`: the section spans the heading line through the next same-or-higher-level heading, or end-of-document. This makes `get --section X` and `edit --replace-section X` agree on what "the X section" means — a read mirrors a write.

- **`--format json`/`jsonl`** add a `sections` object keyed by the requested heading text, e.g. `{"Task Description": "## Task Description\n..."}`. The object is a keyed lookup — keys are unordered (alphabetical), not request-ordered.
- **`--format records`** (the TTY default) prints each requested section as its own labeled block, in request order.
- The section content is byte-identical across formats (the verbatim span, heading line through end-of-section), so a value read from either round-trips to `edit --replace-section`.
- **`--format paths`/`markdown`** ignore `--section` entirely (no section resolution runs), like they ignore `--col`, with a stderr warning — the lookup still succeeds and exits 0.
- Repeating the same heading collapses to one entry (both formats agree on cardinality).
- A heading that's missing or ambiguous (matches more than one heading) in a given document warns on stderr and is simply omitted from that document's `sections` — it does not affect other requested headings or other targets. If *none* of the requested headings resolve for a document, that counts toward `get`'s normal nonzero-exit contract for an unresolved target, but the document's record is still returned.

## Sorting and paging

`get` accepts `--sort`, `--desc`, `--limit`, `--no-limit`, and `--starts-at` over the named targets. Unlike `find`, `get` returns **every** named target by default — no implicit limit.

## Output formats

| Format | Shape | Stable contract |
|---|---|---|
| `records` | Vertical key-value block per document. TTY default. | No. |
| `paths` | One vault-relative path per line. | Yes. |
| `json` | A JSON array of document records, one per resolved target. (Unlike `find`, `get` emits a bare array, not a `{ total, … }` wrapper.) | Yes, versioned. |
| `jsonl` | One JSON object per line. | Yes. |
| `markdown` | The exact UTF-8 source file read from disk, with no trailing-newline fixup. One document at a time. | `get`-only. |

When a matching `norn serve` daemon is live, `markdown` routes through
`vault.get` like the structured formats; the daemon reads the resolved file at
request time and returns the same content. Markdown is a representation,
not a structural facet: it never adds `.raw` or `.source` to records.

## See also

- [`find`](find.md) — select a set of documents by predicate, with the same `--col` vocabulary.
- [`set`](set.md) — update a document's frontmatter or body.
- [`validate`](validate.md) — check documents against configured rules.
- Run `norn get --help` for the full flag reference.
