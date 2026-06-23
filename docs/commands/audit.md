---
title: audit
description: Read the per-vault mutation audit trail — the append-only event stream of every confirmed mutation.
---

# norn audit

Read the per-vault mutation audit trail. Every confirmed mutation (`set`, `edit`, `move`, `delete`, `migrate`, and their `vault.*` MCP equivalents) is appended to an append-only JSONL event stream under the state directory. `audit` is the native reader — so off-filesystem and MCP-only clients don't have to `cat` raw JSONL.

## Examples

```bash
norn audit
# newest 20 events, records format

norn audit --limit 5
# just the 5 most recent

norn audit --trace a1b2c3d4
# all events from one invocation (trace ID prefix match)

norn audit --status applied
# only events that were applied (not skipped or failed)

norn audit --target notes/my-note.md
# events where notes/my-note.md is the source or destination of a move

norn audit --since 2026-06-01 --until 2026-06-15
# events in a UTC day range

norn audit --since 2026-06-22T14:00:00Z
# events since a specific RFC-3339 timestamp

norn audit --format json --limit 10
# structured JSON array of flattened event records

norn audit --raw --limit 5
# the stored OTEL Logs objects verbatim (no norn projection)
```

## Filters

All filters are AND-combined.

| Flag | Description |
|---|---|
| `--trace <ID>` | Match events by trace ID (prefix match). One invocation per trace. |
| `--status applied\|skipped\|failed` | Keep events with the given outcome. |
| `--target <PATH>` | Keep events where `<PATH>` matches the mutation's source or destination (useful for moves). |
| `--since <WHEN>` | Keep events at or after `<WHEN>`. `YYYY-MM-DD` → UTC day start; RFC-3339 timestamp for precision. |
| `--until <WHEN>` | Keep events at or before `<WHEN>`. `YYYY-MM-DD` → UTC day end; RFC-3339 for precision. |
| `--limit <N>` | Return at most `<N>` events. Default: 20. Newest-first. |

## Output

Results are newest-first. The default format is `records` (human-legible key-value blocks); pass `--format json` for a stable JSON array.

### Flattened projection (default)

The default output is a **norn-native flattened projection** of each OTEL Logs record. Hot fields are promoted to top-level; the rest go into a generic `attributes` bag.

| Field | Description |
|---|---|
| `timestamp` | ISO-8601 timestamp of the event. |
| `trace` | Trace ID — one per norn invocation. |
| `span` | Span ID. |
| `severity` | Log severity level (e.g. `INFO`). |
| `event` | Event name with the `norn.` prefix stripped (e.g. `set.applied`). |
| `body` | Human-readable event description. |
| `status` | Outcome: `applied`, `skipped`, or `failed`. |
| `target` | The document path that was mutated (source path for moves). |
| `target_to` | Destination path (moves only). |
| `attributes` | Generic bag of remaining `norn.*` attributes (prefix stripped, dots→underscores). |

### Raw OTEL passthrough (`--raw`)

`--raw` returns the stored OTEL Logs objects verbatim — no norn projection. Use this when you need the full attribute set or when piping to OTEL-aware tooling.

## What is and isn't recorded

Only **confirmed** mutations are recorded — dry-runs, read operations, and cancelled mutations are not written to the stream. An empty or absent event stream returns `[]` with exit 0.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Success (including empty result). |
| `2` | Bad filter argument — e.g. an unparseable `--since`/`--until` value. |

## See also

- [`vault.audit`](../mcp-server.md) — the MCP equivalent, capability-isomorphic with this command. Available under `norn mcp --read-only`.
- Run `norn audit --help` for the full flag reference.
