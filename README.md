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
vault graph unresolved --root <path> --format jsonl
vault graph diagnostics --root <path> --format jsonl
vault graph backlinks <path-or-stem> --root <path> --format jsonl
vault graph inspect <path-or-stem> --root <path> --format json
```

The first pass is stateless and read-only. It walks Markdown files, parses generic frontmatter, extracts headings, extracts Markdown links and wikilinks, and resolves links against vault-relative paths or unique note stems. Exact path lookup is case-sensitive; stem lookup is case-insensitive.
