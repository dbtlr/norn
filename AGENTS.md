# Agent Guide

## Project Shape

This repo builds `vault`, a Rust CLI for deterministic Markdown vault graph/index operations.

Workspace crates:

- `crates/vault-core` — serializable graph data types and diagnostics.
- `crates/vault-frontmatter` — YAML frontmatter extraction and shallow property/offset utilities.
- `crates/vault-links` — CommonMark link parsing, wikilink parsing, block IDs, anchor helpers, and link resolution.
- `crates/vault-graph` — vault walking, build/index entry points, SQLite cache, and pattern matching. Depends on vault-frontmatter and vault-links.
- `crates/vault-standards` — validate engine, config types, findings (`Finding` sum type), summary, predicates, and YAML config-schema validator. Depends on vault-graph.
- `crates/vault-cli` — `clap` command surface for the `vault` binary. Depends on vault-graph and vault-standards.

The binary package is `vault-cli`; the installed command is `vault`.

## Tooling

Use `mise` for repo tools:

```bash
mise install
mise exec -- just verify
```

Declared tools live in `mise.toml`:

- Rust `1.95.0`
- `just`

Useful commands:

```bash
mise exec -- just build
mise exec -- just test
mise exec -- just verify
mise exec -- just run -C fixtures/basic docs list --format jsonl
```

If `just` is not on PATH, use `mise exec -- just ...`. Direct Cargo commands also work:

```bash
cargo build -p vault-cli
cargo test
cargo fmt --check
```

Build outputs:

- debug binary: `target/debug/vault`
- release binary: `target/release/vault`
- install path from `cargo install --path crates/vault-cli`: usually `~/.cargo/bin/vault`

## Current CLI Surface

Core commands:

```bash
vault docs list --format jsonl
vault docs list --filter status:draft --format jsonl
vault links list --format jsonl
vault files --format jsonl
vault links unresolved --format jsonl
vault links backlinks <path-or-stem-or-file> --format jsonl
vault docs inspect <path-or-stem> --format json
vault cache build --cache .vault/cache --format json
vault validate --format jsonl
vault validate --summary --format json
vault -C <path> validate --summary --format json
```

Commands run against the current directory by default. Use global `-C, --cwd <dir>` to run against another vault directory. When `--config` is omitted, `vault` discovers `<cwd>/.vault/config.yaml` if it exists; missing discovered config is fine and uses defaults. Explicit relative `--config` paths and relative `--cache` paths resolve against the effective cwd.

All commands accept global `--config <path>` for explicit YAML configuration. Current config shape:

```yaml
files:
  ignore:
    - "**/__pycache__/**"
    - "**/*.pyc"
validate:
  ignore:
    - "Archive/**"
    - "System/Templates/**"
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
      field_types:
        created: datetime
        modified: datetime
        aliases: list_of_strings
        workspace: wikilink
    - name: task-status
      match:
        path: "**/*.md"
        frontmatter:
          type: task
      required_frontmatter:
        - status
      allowed_values:
        status:
          - backlog
          - in_progress
          - completed
          - wont_do
      allowed_paths:
        - "Workspaces/**/tasks/*.md"
    - name: agent-artifact
      match:
        path: "**/*.md"
        frontmatter:
          type: agent-artifact
      forbidden_frontmatter:
        - kind
      allowed_paths:
        - "Workspaces/**/agent-artifacts/*.md"
```

For the conceptual model of validate rules, see [docs/rule-shape.md](docs/rule-shape.md).

Ignore patterns, validate-only ignore patterns, scoped validate `match.path` / `match.path_not` values, `exclude.path`, and `allowed_paths` are applied to vault-relative paths. `*` matches within one path segment only, and `**` matches zero or more complete path segments. Build summaries include `ignored_files` so count changes are visible.

Ignored targets remain outside the graph. If an indexed Markdown document links to an ignored file, that link is reported as unresolved rather than hidden.

`vault validate` is read-only. It reports unresolved links, ambiguous links, document diagnostics, configured missing frontmatter fields, invalid frontmatter field types, forbidden frontmatter fields, path-location violations, and configured disallowed frontmatter values. Global `validate.required_frontmatter` applies to every document not skipped by `validate.ignore`. Scoped `validate.rules` apply additional requirements only to documents matched by `match.path`, `match.path_not`, and `match.frontmatter`; findings include `rule` when a scoped rule produced them. Frontmatter predicates and `allowed_values` are top-level, exact, and type-sensitive; missing fields do not match allowed-value checks. `field_types` checks only run when a field is present and supports `datetime`, `date`, `list_of_strings`, `wikilink`, and `wikilink_or_list`. `forbidden_frontmatter` reports present forbidden fields. `allowed_paths` reports matching documents outside permitted path patterns. Rule-level `exclude.path` skips a path subset for that rule without removing files from the graph. Unknown `match.*` keys should remain config errors so typoed rules do not broaden silently. `vault validate --summary` emits grouped counts by code, severity, rule, frontmatter field, disallowed field value, and top-level path prefix instead of raw findings. Do not add mutation behavior to validate; use future plan/apply commands for edits.

Lookup rules:

- exact vault-relative paths are case-sensitive
- exact file paths are accepted by `links backlinks`, including non-Markdown attachments
- unique stem lookup is case-insensitive
- stem lookup only applies to Markdown documents
- ambiguous stem lookup exits with an error listing candidates

`--filter` is currently frontmatter-only. Do not silently reinterpret `path`, `stem`, or `dir` as graph-native filter fields until the query model is deliberately expanded.

## Product Principles

The raw graph should be Obsidian-compatible before it is Atlas-opinionated.

In the no-schema baseline, parse what Obsidian treats as internal links:

- body wikilinks
- embeds
- frontmatter/property wikilinks
- URL-decoded Markdown internal links
- extensionless Markdown note links
- heading anchors
- block references
- same-note heading/block references such as `[[#Heading]]` and `[[#^block-id]]`
- Markdown image links to local files
- existing non-Markdown attachment targets

Frontmatter link extraction is shallow in v0.x. It scans top-level scalar strings and top-level lists of strings, preserving the top-level property name in `source_context.property` and adding `source_span` for those shallow cases. Do not assume nested YAML leaves are graph links until that boundary is deliberately expanded.

Future standards packs should layer semantic meaning on top of the raw graph. For example, `workspace: "[[vault-cli]]"` and a prose body link are both raw links, but they are different semantic relationships.

Keep destructive operations out of the graph/index layer for now. Refactor and apply commands should eventually default to dry-run and require explicit apply behavior.

## Test Fixtures

The main fixture vault is `fixtures/basic`.

It intentionally covers:

- generic YAML frontmatter
- malformed frontmatter diagnostics
- headings and block IDs
- Markdown links
- URL-encoded Markdown links
- extensionless Markdown links
- body wikilinks
- embeds
- frontmatter/property wikilinks
- same-note heading/block links
- duplicate stems / ambiguous links
- path-qualified wikilinks with case differences
- Markdown image links to local files
- non-Markdown attachments
- ignored wikilinks in inline code and fenced code

When changing output schemas or parsing behavior, update `crates/vault-cli/tests/cli_output.rs` and run:

```bash
mise exec -- just verify
```

## SQLite Cache

`vault cache build` writes a SQLite projection. The cache is an implementation detail during v0.x; CLI commands should remain the primary query surface.

Cache behavior:

- `--cache some/dir` writes `some/dir/graph.sqlite`
- `--cache some/file.sqlite` writes that file directly
- `--format` only controls stdout
- cache schema can evolve during v0.x

The cache has a `metadata` table with `schema_version`.

## Versioning

Use semver-style tags for milestones:

- `v0.1.0` — initial graph CLI baseline
- `v0.2.0` — contract polish and diagnostics
- `v0.3.0` — Obsidian-compatible graph semantics
- `v0.4.0` — same-note references, Markdown image links, file queries, and graph contract polish
- `v0.5.0` — explicit graph ignore config and mutation-ready frontmatter spans
- `v0.6.0` — read-only validate reports
- `v0.7.0` — scoped validate rules
- `v0.8.0` — path-segment glob semantics and config validation
- `v0.9.0` — frontmatter-aware validate rule matching
- `v0.10.0` — validate command rename and summary output
- `v0.11.0` — allowed frontmatter value validation
- `v0.12.0` — global cwd and default config discovery
- `v0.13.0` — broken-pipe handling and richer validate summaries
- `v0.14.0` — standards-pack dogfood expressiveness

For a release bump:

1. Update workspace version in `Cargo.toml`.
2. Run `cargo check` to update `Cargo.lock`.
3. Run `mise exec -- just verify`.
4. Commit `Cargo.toml` and `Cargo.lock`.
5. Tag with `git tag -a vX.Y.Z -m "vault-cli vX.Y.Z"`.

## Git Practice

Commit coherent milestones as work progresses. Keep unrelated vault-note edits separate from repo code commits unless the user explicitly asks to track them in this repo.

Do not commit `agents.local.md`; it is local-machine guidance and is ignored.
