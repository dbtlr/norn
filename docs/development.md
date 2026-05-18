---
title: Development
description: Local build, test, and verification workflow for contributors using mise and just, plus the MSRV policy.
---

# Development

This page is for contributors working on `vault-cli` itself. If you're a user looking to install the binary, see [installation.md](installation.md).

## Toolchain

The repo uses [mise](https://mise.jdx.dev/) to pin tool versions. Declared tools live in `mise.toml`:

- Rust (latest stable)
- `just`

Install them:

```bash
mise install
```

If `just` is not on your `PATH`, prefix commands with `mise exec --`:

```bash
mise exec -- just build
```

Direct Cargo commands also work:

```bash
cargo build -p vault-cli
cargo test --workspace
cargo fmt --check
```

## Common recipes

```bash
mise exec -- just build      # cargo build -p vault-cli
mise exec -- just test       # cargo test --workspace
mise exec -- just verify     # fmt --check + clippy + test
mise exec -- just run -C fixtures/basic docs list --format jsonl
```

Build outputs:

- Debug binary: `target/debug/vault`
- Release binary: `target/release/vault`
- `cargo install --path crates/vault-cli` installs to `~/.cargo/bin/vault`

## Workspace layout

```
crates/
  vault-core/         # serializable graph types and diagnostics
  vault-frontmatter/  # YAML frontmatter extraction and offset utilities
  vault-links/        # CommonMark + wikilink parsing, block IDs, anchors, resolution
  vault-graph/        # vault walking, build/index entry points, pattern matching
  vault-standards/    # validate engine, config types, findings, summary, repair
  vault-cli/          # clap command surface for the `vault` binary
```

`vault-cli` depends on `vault-graph` and `vault-standards`. The pure-parsing crates (`vault-core`, `vault-frontmatter`, `vault-links`) have no upstream dependencies on each other beyond `vault-core` and are unit-tested in isolation.

## MSRV policy

The minimum supported Rust version is **latest stable**. `rust-version` in `crates/vault-cli/Cargo.toml`, the toolchain pin in `mise.toml`, and the `dtolnay/rust-toolchain` action in CI move in lockstep.

When a new Rust stable lands, bump all three in one commit, then update the CHANGELOG.

## Test fixtures

The main fixture vault is `fixtures/basic`. It intentionally covers:

- generic YAML frontmatter
- malformed frontmatter diagnostics
- headings and block IDs
- Markdown links (regular, URL-encoded, extensionless)
- body wikilinks
- embeds
- frontmatter/property wikilinks
- same-note heading/block links
- duplicate stems / ambiguous links
- path-qualified wikilinks with case differences
- Markdown image links to local files
- non-Markdown attachments
- ignored wikilinks in inline code and fenced code

Integration tests live at `crates/vault-cli/tests/cli_output.rs`. When changing output schemas or parsing behavior, update those tests and run:

```bash
mise exec -- just verify
```

## Commit and PR practice

- Keep commits atomic and focused. Conventional commit messages encouraged but not enforced.
- Every versioned feature or behavior change updates `CHANGELOG.md` in the same work slice. Don't leave new behavior under an older release heading.
- Don't commit `agents.local.md`; it's local-machine guidance and ignored by `.gitignore`.

## Contributing

See [CONTRIBUTING.md](../CONTRIBUTING.md) for the contribution intent and PR process. For security issues, see [SECURITY.md](../SECURITY.md).

## See also

- [Releases and versioning](releases.md) — release workflow.
- [Agent workflows](agent-workflows.md) — agent-facing contract (some contributors may want to keep this stable too).
