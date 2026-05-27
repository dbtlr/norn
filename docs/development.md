---
title: Development
description: Local build, test, and verification workflow for contributors using mise and just, plus the MSRV policy.
---

# Development

This page is for contributors working on `norn` itself. If you're a user looking to install the binary, see [installation.md](installation.md).

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
cargo build -p norn
cargo test --workspace
cargo fmt --check
```

## Common recipes

```bash
mise exec -- just build      # cargo build -p norn
mise exec -- just test       # cargo test --workspace
mise exec -- just verify     # fmt --check + clippy + test
mise exec -- just run -C fixtures/basic find --all --format jsonl
```

Build outputs:

- Debug binary: `target/debug/norn`
- Release binary: `target/release/norn`
- `cargo install --path .` installs to `~/.cargo/bin/norn`

## Crate layout

`norn` is a single-crate repo. The previous workspace layout (six internal library crates plus the `norn` bin) was collapsed in v0.34 — the former crates now live as modules under `src/`:

```
src/
  core/         # serializable graph types and diagnostics
  frontmatter/  # YAML frontmatter extraction and offset utilities
  links/        # CommonMark + wikilink parsing, block IDs, anchors, resolution
  graph/        # vault walking, build/index entry points, pattern matching
  standards/    # validate engine, config types, findings, summary, repair
  cache/        # SQLite-backed graph cache
  cli.rs        # clap command surface for the `norn` binary
  main.rs
```

The pure-parsing modules (`core`, `frontmatter`, `links`) still depend on each other only through `core` and are unit-tested in isolation.

## MSRV policy

The project tracks **latest stable** Rust. The toolchain pin in `mise.toml` and the `dtolnay/rust-toolchain` action in CI move in lockstep when a new stable lands; update both in one commit and note the bump in the CHANGELOG.

`rust-version` is intentionally omitted from `Cargo.toml` for now. Cargo-dist's release builders (notably `aarch64-unknown-linux-musl`) ship rustc versions that lag the latest stable by several months, and declaring a high MSRV would reject those builders even though the actual code compiles cleanly. The field will be re-added when norn commits to publishing on crates.io and needs to advertise a stable MSRV contract.

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

Integration tests live at `tests/cli_output.rs`. When changing output schemas or parsing behavior, update those tests and run:

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
