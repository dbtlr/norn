# Agent Guide

## Project Shape

This repo builds `vault`, a Rust CLI for deterministic Markdown vault graph/index operations.

Workspace crates:

- `crates/vault-core` — serializable graph data types and diagnostics.
- `crates/vault-index` — stateless vault walking, Markdown/frontmatter parsing, link resolution, and SQLite cache writing.
- `crates/vault-cli` — `clap` command surface for the `vault` binary.

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
mise exec -- just run graph documents --root fixtures/basic --format jsonl
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

Core graph commands:

```bash
vault graph documents --root <path> --format jsonl
vault graph documents --root <path> --filter status:draft --format jsonl
vault graph links --root <path> --format jsonl
vault graph files --root <path> --format jsonl
vault graph unresolved --root <path> --format jsonl
vault graph diagnostics --root <path> --format jsonl
vault graph backlinks <path-or-stem-or-file> --root <path> --format jsonl
vault graph inspect <path-or-stem> --root <path> --format json
vault graph build --root <path> --cache .vault/cache --format json
```

Lookup rules:

- exact vault-relative paths are case-sensitive
- exact file paths are accepted by `graph backlinks`, including non-Markdown attachments
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

Frontmatter link extraction is shallow in v0.x. It scans top-level scalar strings and top-level lists of strings, preserving the top-level property name in `source_context.property`. Do not assume nested YAML leaves are graph links until that boundary is deliberately expanded.

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

When changing output schemas or parsing behavior, update `crates/vault-cli/tests/graph_output.rs` and run:

```bash
mise exec -- just verify
```

## SQLite Cache

`vault graph build` writes a SQLite projection. The cache is an implementation detail during v0.x; CLI commands should remain the primary query surface.

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

For a release bump:

1. Update workspace version in `Cargo.toml`.
2. Run `cargo check` to update `Cargo.lock`.
3. Run `mise exec -- just verify`.
4. Commit `Cargo.toml` and `Cargo.lock`.
5. Tag with `git tag -a vX.Y.Z -m "vault-cli vX.Y.Z"`.

## Git Practice

Commit coherent milestones as work progresses. Keep unrelated vault-note edits separate from repo code commits unless the user explicitly asks to track them in this repo.

Do not commit `agents.local.md`; it is local-machine guidance and is ignored.
