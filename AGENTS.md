# Agent Guide

`vault` is a deterministic Markdown vault CLI for humans and agents. This file is the short agent contract; the full guide is at [docs/agent-workflows.md](docs/agent-workflows.md).

## How We Work

- **Never push to main.** All work should be done in a branch or worktree and
  pushed as a PR.
- **Small meaningful commits.** Create useful checkpoints on long tasks.
- **Discuss first, code second.** Align on package boundaries and user-facing
  behavior before large implementation changes.
- **No broken windows.** Fix errors and warnings encountered while working.
- **Documentation as a deliverable.** No task is complete without accompanying documentation.

## Quick rules

- `vault validate`, `vault find`, `vault count`, `vault get`, `vault repair plan`, and `vault config show` are read-only.
- Repair flows are explicit: `vault repair plan` produces an artifact; `vault repair apply` consumes it.
- Stable contracts: JSON for human/dispatch, JSONL for streams, schema-versioned repair plans (`schema_version: 8`).
- Apply rejects plans for a different vault root, stale document hashes, or unsupported schema versions.

## Quick start for agents

1. Detect the vault root: `-C <path>` (or `--cwd <path>`). If neither is set, `vault` runs against the current directory and discovers `.vault/config.yaml` if present.
2. Start with `vault validate --summary --format json`.
3. Filter findings for a triage queue with `--code`, `--severity`, `--field`, `--rule`, `--path`, `--target`, `--reason`.
4. For single-document frontmatter mutation, use `vault set <doc> --field KEY=VALUE --dry-run` (preview) then `--yes` (apply). For batch finding-driven repairs, write a plan: `vault repair plan --out repair.json`. Apply with `vault repair apply repair.json --dry-run` then `--verify`.

## Documentation First

- All new tasks should include updating any related documentation to updates that are made.

## Architecture Documentation

- Architecture docs live in `docs/architecture/`.
- When adding or changing a system component, update the relevant doc.
- When adding a new subsystem, create a new doc for it.
- Docs explain what, why, and how — not just API signatures.
- Use Mermaid diagrams to visualize data flows, component relationships, and system architecture.

## CLI Documentation

- CLI docs live in `docs/commands/`.
- When adding a new command, create a doc page and add it to the commands README.
- When modifying command flags or behavior, update the corresponding doc page.
- Each doc page follows the template: description, usage, options table, examples, see also.
- Examples should show brief JSON output (1-2 records) demonstrating the shape, not exhaustive results.

## Documentation

- [docs/agent-workflows.md](docs/agent-workflows.md)
- [docs/validation.md](docs/validation.md)
- [docs/rule-shape.md](docs/rule-shape.md)
- [integrations/agent-skill/README.md](integrations/agent-skill/README.md)

## Developer guide

For contributors working on `vault` itself (build, test, release), see [docs/development.md](docs/development.md). This file intentionally stays short and agent-focused.
