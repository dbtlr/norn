---
title: Command reference
description: Index of every norn command, each linking to its own reference page.
---

# Command reference

Every norn command, each linking to its own page with examples, options, and output contracts. Run `norn <command> --help` for the authoritative, always-current flag list (`-h` for the compact version).

## Global flags

| Flag | Description |
|---|---|
| `-C, --cwd <dir>` | Run against `<dir>` instead of the process current directory. |
| `--config <path>` | Explicit `.norn/config.yaml` path. Relative paths resolve against the effective cwd. |
| `--verbose` | Verbose stderr logging. |
| `--no-cache-refresh` | Skip the implicit cache refresh before reading the graph. |
| `--color <when>` | Color output. Honors `NO_COLOR` / `CLICOLOR_FORCE`. |

When `--config` is omitted, norn discovers `<cwd>/.norn/config.yaml` if it exists; a missing discovered config is fine and uses defaults.

## Query and read

| Command | Summary |
|---|---|
| [`find`](commands/find.md) | Find documents by frontmatter, body text, path, or link relationship. |
| [`count`](commands/count.md) | Count documents, total or grouped, over the find filter surface. |
| [`get`](commands/get.md) | Get one or more documents in full — frontmatter, headings, links. |
| [`audit`](commands/audit.md) | Read the per-vault mutation audit trail (append-only event stream). |

## Create and mutate

| Command | Summary |
|---|---|
| [`new`](commands/new.md) | Create a document with frontmatter pre-filled from path rules. |
| [`set`](commands/set.md) | Update one document's frontmatter or body. |
| [`edit`](commands/edit.md) | Edit one document's body with atomic, content-anchored partial edits. |
| [`move`](commands/move.md) | Move or rename a document, rewriting incoming links. |
| [`delete`](commands/delete.md) | Delete a document, optionally redirecting its backlinks. |
| [`rewrite-wikilink`](commands/rewrite-wikilink.md) | Retarget a wikilink across the vault without moving a file. |

## Validate and repair

| Command | Summary |
|---|---|
| [`validate`](commands/validate.md) | Validate the vault against configured rules and graph facts. |
| [`repair`](commands/repair.md) | Turn findings into an inspectable MigrationPlan. |
| [`migrate`](commands/migrate.md) | Apply a MigrationPlan with precondition checks. |

## Setup and maintenance

| Command | Summary |
|---|---|
| [`init`](commands/init.md) | Scaffold a `.norn/config.yaml`. |
| [`config`](commands/config.md) | Show, validate, migrate, or edit the config. |
| [`cache`](commands/cache.md) | Manage the SQLite vault graph cache. |
| [`completions`](commands/completions.md) | Install shell completions. |
| [`self-update`](commands/self-update.md) | Update norn to the latest GitHub release. |

## See also

- [Configuration](configuration.md) — every config key.
- [Validation and repair](validation.md) — finding codes and recipes.
- [Agent workflows](agent-workflows.md) — the stable JSON/JSONL contracts.
