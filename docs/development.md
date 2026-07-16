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

## Architecture map

`norn` is a single crate. The old workspace layout (six internal library crates plus the `norn` bin) was collapsed into modules under `src/` in v0.34. This section is the map: where a request goes, where the three surfaces meet, and which file to open when you need to change behavior. If you are here to add or change a command, read the request lifecycle, then jump to [Adding a command](#adding-a-command).

### The request lifecycle

A CLI invocation walks a fixed path from `argv` to rendered output. Follow it in the code and the layering falls out.

1. **`argv` → `grammar` normalization.** `grammar::normalize_argv` (`src/grammar.rs`) runs before clap sees anything. It resolves command aliases and desugars dynamic `--key value` predicates into canonical `--eq` / `--in` (ADR 0010). A canonical invocation passes through byte-identical. This happens in `cli_main` in `src/lib.rs`.
2. **clap parse.** The normalized argv feeds the derive-generated command tree in `src/cli.rs` (`Cli::command()` → `from_arg_matches`). `cli.rs` is the whole flag surface: the `Command` enum, every `*Args` struct, and every `*Format` value enum. It holds no logic.
3. **`lib.rs::run` dispatch.** `run` in `src/lib.rs` is the hub. It opens config and cache, then matches the parsed `Command` to one arm per verb. The arm is where a command's CLI-direct behavior lives, or where it delegates into the command module.
4. **Command module.** Real work lives at `src/<verb>/` (`src/find/`, `src/set/`, `src/move/`, and so on). The module's `mod.rs` is the entry; internal files split by job (see [Command module convention](#command-module-convention)).
5. **Report → render.** A read produces a report or record set that a `render.rs` turns into text/JSON/JSONL. A mutation first synthesizes a plan, applies it, and produces an `ApplyReport` (`src/apply_report.rs`) that renders to the same envelope whether the mutation ran direct or through the daemon.

Exit codes are mapped at the two ends. `run` returns an `i32` that `cli_main` hands to `process::exit`; a broken pipe becomes 0, an `Err` prints and exits 1, and a refusal returns its own code (commonly 2) from inside the arm.

### The three surfaces

The same capability core is reachable three ways, and they converge on one set of handlers.

- **CLI-direct.** The `run` match arms in `src/lib.rs`. This is the path above: parse, open cache, do the work in-process.
- **MCP.** `src/mcp/` serves the Model Context Protocol. Each tool is a pure handler in `src/mcp/tools/<verb>.rs` (`vault.find`, `vault.set`, …) plus a thin `#[tool]` wrapper in `src/mcp/server.rs`. The handler runs the same underlying code path the CLI arm does. `norn mcp` serves it cold over stdio.
- **Routed / daemon.** A warm `norn serve` daemon can answer a request without re-opening the cache. `run` calls a route seam early — `try_route_read` for reads, `try_route_<verb>` for mutations (`try_route_set`, `try_route_move`, and the rest), plus `route_count` / `route_find` / `route_get` / `route_repair`. The seam probes the control socket through `src/service/` (the client). If a daemon is live it forwards the call, `src/serve/` (the daemon) dispatches it to the **same MCP handlers**, and the seam returns `Some(result)`. No daemon, or a forced-direct flag, returns `None` and execution falls through to the CLI-direct arm. `src/route_wire.rs` owns the arg→MCP-parameter translation so the routed and direct shapes cannot drift.

`src/mcp/parity_gate.rs` keeps the CLI and MCP surfaces honest. It is a compile-time-enforced test (ADR 0009): it walks the derive-generated clap tree and the served MCP tool router, and fails the build if a CLI flag has no MCP twin and no allowlisted carve-out. Its `specs()` table is the hand-declared mapping of which CLI command backs which MCP tool, which fields are renamed, and which gaps are known and tracked. Every command must appear there.

### The two refusal seams

A typed precondition failure (a CAS mismatch, a containment violation, a preflight refusal) carries a stable machine `code`. Two seams turn that typed error into a coded envelope, one per surface:

- **`ApplyError::from_anyhow`** in `src/apply_report.rs` — the CLI `--format json` path. It always produces an envelope, falling back to `internal-error` for anything it does not recognize.
- **`mcp::mutate::refusal_from_error`** in `src/mcp/mutate.rs` — the MCP single-op path. It returns `Some` only for recognized refusals and `None` for genuine internal failures, so those stay a bare error instead of being laundered into a misleading refusal.

These two downcast ladders are maintained by hand and must stay in sync. A new typed refusal family has to be registered in **both** seams (plus a row in `docs/errors.md`) until the `CodedError` trait lands and collapses them into one (NRN-236). The error-authoring checklist above each function spells out the steps.

### Module directory

One line per top-level module. The command modules (`find/`, `count/`, `get/`, `set/`, `edit/`, `new/`, `move/`, `delete/`, `apply/`, `repair/`, `rewrite_wikilink/`, `validate/`, `describe/`, `audit/`) share the convention below and are omitted individually.

Dispatch and parsing:

- `lib.rs` — the dispatch hub: `cli_main`, `run`, the arm-per-command match, the route seams, exit-code mapping.
- `cli.rs` — the clap command surface (`Command` enum, `*Args`, `*Format`). Declarations only.
- `grammar.rs` — the pre-clap forgiving-input pass (alias resolution, dynamic-predicate desugaring).
- `config_loader.rs` — resolves the vault root and `.norn/config.yaml`, produces the `LoadedConfig` commands share.

Engines:

- `cache/` — the SQLite-backed graph cache: schema, writer, readers, freshness, prune, EAV field index.
- `frontmatter/` — YAML frontmatter extraction, offsets, and style-preserving serialization.
- `graph/` — vault walking, index build, alias resolution, glob pattern matching.
- `links/` — CommonMark and wikilink parsing, block IDs, anchors, link resolution.
- `standards/` — the validate engine, config types, findings, summary, and repair rules.
- `planner/` — turns findings and mutation intents into a `MigrationPlan`.
- `applier.rs` — applies a plan to the vault; `repair_apply.rs` orchestrates the repair-specific apply passes.

Surfaces:

- `mcp/` — the MCP server, tool handlers (`tools/`), the mutate/refusal seam, and the parity gate.
- `serve/` — the warm host daemon: socket ownership, accept loop, per-vault warm contexts.
- `service/` — the CLI-side routing client that probes the daemon and forwards a routable call.

Support:

- `output/` — rendering primitives: palette, glyphs, pager, JSON/column projection.
- `help/` — the `-h`/`--help` interception and example extraction.
- `telemetry/` — the local event store (opt-in, deterministic, no network).
- `mutation_lock/` — the per-vault advisory lock every mutating surface acquires.
- `seq_alloc.rs` — allocates `{{seq}}` counters for `new`-created documents.
- `route_wire.rs` — the shared arg→MCP-parameter translation for the routed command seams.

Pure-parsing modules (`core`, `frontmatter`, `links`) depend on each other only through `core` and are unit-tested in isolation. `core/` holds the serializable graph types and diagnostics.

### Command module convention

Every CLI command lives at `src/<verb>/mod.rs`, with the module name matching the CLI verb exactly. `norn move` is `src/move/mod.rs`, declared with the raw identifier `r#move` because `move` is a reserved keyword. Internal files within a command module follow a `route.rs` / `render.rs` / `synth.rs` / `report.rs` naming pattern, present only where the command needs them: `route.rs` for daemon-routing translation, `render.rs` for output rendering, `synth.rs` for plan synthesis, `report.rs` for report types.

The bare `foo.rs` plus sibling `foo/` idiom is reserved for the large non-command engines (`cache.rs` + `cache/`, `frontmatter.rs` + `frontmatter/`, `graph.rs` + `graph/`), which aren't CLI commands and don't need a verb-shaped name. New commands follow the `src/<verb>/mod.rs` scheme from the start, not the older `_cmd`/`_doc`-suffixed naming a few modules historically used.

### Adding a command

Adding a verb touches a predictable set of files. Working outward from the parse layer:

1. **`src/cli.rs`** — add the `Command` enum variant and its `*Args` struct (and a `*Format` value enum if the command renders more than one shape).
2. **`src/<verb>/mod.rs`** — create the module with the command's logic; add `route.rs` if it will route to the daemon, and the other convention files as needed.
3. **`src/lib.rs`** — add the `run` match arm, and the route seam call (`try_route_<verb>` or a `route_<verb>` helper) if the command is routable.
4. **`src/mcp/tools/<verb>.rs`** — add the MCP handler, wire its `#[tool]` wrapper in `src/mcp/server.rs`, and register the command in `parity_gate.rs`'s `specs()` (as an `Mcp` mapping, or a justified `LocalOnly` / `TrackedGap`). Skipping the parity registration fails the build.
5. **Refusal codes** — if the command raises a new typed refusal, register it in both `ApplyError::from_anyhow` and `mcp::mutate::refusal_from_error`, and add the row to `docs/errors.md`.
6. **Tests** — `tests/<verb>_command.rs` (CLI behavior), `tests/mcp_<verb>.rs` (MCP handler), and `tests/serve_<verb>_routing.rs` (routed↔direct equivalence) if routable. A few existing files fold two verbs together (`serve_find_get_routing.rs`) or abbreviate (`mcp_rewrite.rs`); match the closest existing pattern.
7. **`docs/commands/<verb>.md`** — the reference page for the new command.

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
