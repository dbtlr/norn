---
title: Concepts
description: The deterministic graph, document inventory, frontmatter model, Obsidian-compatible link semantics, and the validate-vs-repair boundary.
---

# Concepts

`vault` is built around a few small ideas. This page explains them once so the command reference and validation guide can stay tight.

## The vault graph

A **vault** is a directory containing Markdown files plus their attachments. `vault` walks the directory (honoring `files.ignore` patterns from `.vault/config.yaml`) and produces a deterministic graph.

The graph contains:

- **Documents.** Markdown files (`*.md`) with parsed frontmatter, headings, and body text.
- **Links.** Every Markdown link, wikilink, embed, and image reference between documents and files. Each link carries the source path, target string, kind, status (resolved / unresolved / ambiguous), and source span. Non-Markdown attachments (`*.png`, `*.pdf`, etc.) are tracked internally as link targets so attachment references can resolve, but they are not first-class graph nodes.

Graph construction is read-only and stateless. The same vault produces the same graph on every run.

## Document inventory

`vault find` is the inventory and search query. Predicates:

- `--path "<glob>"` — vault-relative path glob (path-segment semantics; see [Glob matching](#glob-matching) below).
- `--eq field:value` / `--not-eq field:value` — frontmatter equality / inequality.
- `--in field:v1,v2` / `--not-in field:v1,v2` — set membership.
- `--has <field>` / `--missing <field>` — frontmatter field presence.
- `--text <substring>` — case-insensitive body text substring.

All predicates are ANDed. Pass `--all` with no other predicates to return every document.

`vault count --by <field>` produces grouped counts for a single frontmatter field. Without `--by`, emits the total. Use it to size a queue before listing.

`vault get <path-or-stem>` returns one document's detail: frontmatter, headings, outgoing links, unresolved links, and incoming links.

## Frontmatter

`vault` parses YAML frontmatter at the top of each Markdown file. Top-level scalar strings and top-level lists of strings are extracted to the graph; nested YAML objects and lists are parsed for validation purposes but are not surfaced as graph links (the link-extraction layer is intentionally shallow in v0.x).

Frontmatter validation lives in `validate.rules` in `.vault/config.yaml`. See [validation.md](validation.md) and [rule-shape.md](rule-shape.md) for the rule model.

## Links: Obsidian-compatible

The link parser aims to match Obsidian's internal-link behavior before any standards-pack semantics are applied. It recognizes:

- Body wikilinks: `[[Note Name]]`, `[[Note Name|alias]]`
- Embeds: `![[Note Name]]`, `![[Assets/diagram.png]]`
- Frontmatter/property wikilinks (top-level scalar and list-of-string values that look like wikilinks)
- URL-decoded Markdown internal links: `[text](Encoded%20Path.md)`
- Extensionless Markdown note links: `[text](Note Name)` (resolves if the stem is unique)
- Heading anchors: `[[Note#Heading]]`, `[text](Note.md#Heading)`
- Block references: `[[Note#^block-id]]`
- Same-note references: `[[#Heading]]`, `[[#^block-id]]`
- Markdown image links to local files: `![Alt](Assets/pic.png)`
- Non-Markdown attachment links that resolve when the target file exists

Wikilinks inside inline code (`` `[[example]]` ``) and fenced code blocks are not graph edges.

### Lookup rules

- **Exact paths** are case-sensitive.
- **Unique stem lookup** is case-insensitive and applies only to Markdown documents.
- **Ambiguous stems** (two documents with the same case-insensitive stem) report as `link-ambiguous` findings with all candidate paths.
- **Backlink queries by exact attachment path** are supported via `vault get <path> --col incoming_links`.

## Validation vs repair

The product loop is four stages:

1. **Detect** drift with graph facts and configured `validate.rules`. Output: findings.
2. **Plan** supported repairs as a JSON artifact. Output: `repair.json`.
3. **Apply** the plan explicitly via `vault repair apply`. Output: modified files + an apply report.
4. **Verify** the vault after changes (`apply --verify`, or another `validate --summary` run).

Validation is read-only and does not guess repairs. Repair planning is read-only and produces only inspectable artifacts. There is no hidden write path.

Two explicit write surfaces exist: `vault repair apply` is the finding-driven batch write path — it requires an explicit plan artifact. `vault set`, `vault move`, and `vault delete` are the operator-driven CRUD surface for direct one-document mutations. Both paths are safe-by-default (dry-run previews, `--yes` to apply) and both go through the same underlying apply machinery.

Repair plans are schema-versioned (`schema_version: 8` as of v0.32). Apply rejects unsupported schema versions, plans for a different vault root, stale document hashes, conflicting field changes, and expected-old-value mismatches.

For the full repair model and supported actions, see [validation.md](validation.md).

## Glob matching

Path globs (in `files.ignore`, `validate.ignore`, `match.path`, `match.path_not`, `allowed_paths`, `exclude.path`, and `--path` flags) use path-segment semantics:

- `*` matches within one path segment only.
- `**` matches zero or more complete path segments.
- `docs/*.md` matches `docs/intro.md` but not `docs/guides/intro.md`.
- `docs/**/*.md` matches Markdown files at any depth under `docs/`.
- `docs/**/notes/*.md` matches files directly inside any `notes/` directory under `docs/`, but not files in subdirectories below that `notes/`.

Globs are matched against vault-relative paths with forward-slash separators.

## Output formats

Every command accepts `--format`:

- `json` — single JSON document. Best for one-shot agent dispatch.
- `jsonl` — one JSON object per line. Best for streaming and large result sets.
- `table` — human-readable columns. The schema is not stable across point releases.
- `paths` — one vault-relative path per line. Best for piping into `xargs`, `fzf`, etc.

When `--format` is omitted, commands with a human renderer default to `table` on a terminal and `json` when stdout is piped or captured. Pass an explicit `--format` for stable contracts.

## Next

- [Commands](commands.md) — the full subcommand surface.
- [Configuration](configuration.md) — `.vault/config.yaml` schema.
- [Validation and repair](validation.md) — the detect/plan/apply/verify loop in detail.
