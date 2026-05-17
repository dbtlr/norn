# vault-cli

Experimental Rust CLI for deterministic Markdown vault graph indexing.

The current binary name is `vault`.

## Build

Install repo tools with `mise`:

```bash
mise install
```

```bash
cargo build -p vault-cli
```

The debug binary is written to:

```bash
target/debug/vault
```

To install it onto your Cargo bin path:

```bash
cargo install --path crates/vault-cli
```

With `just` installed, common commands are available as:

```bash
just build
just test
just verify
just run graph documents --root fixtures/basic --format jsonl
```

## v0 Scope

```bash
vault graph documents --root <path> --format jsonl
vault graph documents --root <path> --filter status:draft --format jsonl
vault graph build --root <path> --cache .vault/cache --format json
vault graph links --root <path> --format jsonl
vault graph files --root <path> --format jsonl
vault graph unresolved --root <path> --format jsonl
vault graph diagnostics --root <path> --format jsonl
vault graph backlinks <path-or-stem-or-file> --root <path> --format jsonl
vault graph inspect <path-or-stem> --root <path> --format json
vault validate --root <path> --config <path> --format jsonl
vault validate --root <path> --config <path> --summary --format json
```

Every graph command accepts `--config <path>` for explicit YAML configuration. The current config shape is:

```yaml
graph:
  ignore:
    - "**/__pycache__/**"
    - "**/*.pyc"
validate:
  required_frontmatter:
    - title
  rules:
    - name: workspace-notes
      match:
        path: "Workspaces/**/notes/*.md"
      required_frontmatter:
        - type
        - kind
        - workspace
    - name: typed-note
      match:
        path: "**/*.md"
        frontmatter:
          type: note
      required_frontmatter:
        - kind
```

Configured ignores are applied before file inventory and document parsing. With no config, the graph remains a raw filesystem view except for hidden files/directories.

Ignored targets remain outside the graph. If an indexed Markdown document links to an ignored file, that link is reported as unresolved rather than hidden.

The first pass is stateless and read-only. It walks Markdown files, parses generic frontmatter, extracts headings, extracts Markdown links and wikilinks, and resolves links against vault-relative paths or unique note stems. Exact path lookup is case-sensitive; stem lookup is case-insensitive.

The raw graph aims to follow Obsidian-style internal link behavior before applying any future standards-pack semantics. It includes body wikilinks, embeds, frontmatter/property wikilinks, URL-decoded Markdown internal links, extensionless Markdown note links, same-note heading/block references, Markdown image links to local files, and existing non-Markdown attachment targets.

Frontmatter link extraction is intentionally shallow in v0.x: it scans top-level scalar string properties and top-level lists of strings. Nested YAML object/list leaves are not graph links until the schema layer or a real vault need makes that boundary worth expanding.

Use `source_context.area` and `source_context.property` to distinguish body links from frontmatter/property links. Frontmatter links now include `source_span` for the shallow extraction cases. `vault graph files` emits the file inventory, and `vault graph backlinks <exact-file-path>` can query incoming links to non-Markdown attachment targets.

`vault validate` is read-only. It reports unresolved links, ambiguous links, document diagnostics, and configured missing frontmatter fields without mutating files. Global `validate.required_frontmatter` applies to every document. Scoped `validate.rules` apply additional requirements only to documents matched by `match.path` and `match.frontmatter`; findings include `rule` when a scoped rule produced them.

Use `vault validate --summary` to emit grouped finding counts instead of raw
findings. Summary output includes total findings plus counts by `code`,
`severity`, `rule`, and top-level path prefix.

## Glob Matching

Config path patterns are matched against vault-relative paths using path-segment
semantics:

- `*` matches within one path segment only.
- `**` matches zero or more complete path segments.
- `Workspaces/*/*.md` matches `Workspaces/app/root.md`, but not
  `Workspaces/app/notes/note.md`.
- `Workspaces/**/*.md` matches markdown files at any depth under `Workspaces`.
- `Workspaces/**/notes/*.md` matches files directly inside a `notes` directory,
  including nested workspace paths, but not files in subdirectories below
  `notes`.

## Validate Rule Matching

Scoped validate rules support path and frontmatter predicates. All predicates are
ANDed. Missing frontmatter fields do not match. Frontmatter predicates use exact,
type-sensitive equality for strings, booleans, and numbers.

```yaml
validate:
  rules:
    - name: note-kind
      match:
        path: "**/*.md"
        frontmatter:
          type: note
      required_frontmatter:
        - kind

    - name: task-status
      match:
        path: "**/*.md"
        frontmatter:
          type: task
      required_frontmatter:
        - status
```
