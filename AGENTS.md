# Agent Guide

`vault` is a deterministic Markdown vault CLI for humans and agents. This file is the short agent contract; the full guide is at [docs/agent-workflows.md](docs/agent-workflows.md).

## Quick rules

- `vault validate` and `vault docs|links|files|search` are read-only.
- Repair flows are explicit: `vault repair plan` produces an artifact; `vault repair apply` consumes it.
- Stable contracts: JSON for human/dispatch, JSONL for streams, schema-versioned repair plans (`schema_version: 3`).
- Apply rejects plans for a different vault root, stale document hashes, or unsupported schema versions.

## Quick start for agents

1. Detect the vault root: `-C <path>` or `--vault <name>` (registered via `vault registry add`).
2. Start with `vault validate --summary --format json`.
3. Filter findings for a triage queue with `--code`, `--severity`, `--field`, `--rule`, `--path`, `--target`, `--reason`.
4. For mutation, write a plan: `vault repair plan --out repair.json`. Apply with `vault repair apply repair.json --dry-run` then `--verify`.

## Documentation

- [docs/agent-workflows.md](docs/agent-workflows.md)
- [docs/validation.md](docs/validation.md)
- [docs/rule-shape.md](docs/rule-shape.md)
- [integrations/agent-skill/README.md](integrations/agent-skill/README.md)

## Developer guide

For contributors working on `vault` itself (build, test, release), see [docs/development.md](docs/development.md). This file intentionally stays short and agent-focused.
