---
title: MCP server
description: Run norn as a Model Context Protocol stdio server — the 14-tool catalog, the dry-run/confirm mutation contract, the document-placement workflow, --read-only, and the warm-cache and call-ordering notes.
---

# MCP server

`norn mcp` runs a [Model Context Protocol](https://modelcontextprotocol.io/) server over stdio, exposing the vault to any MCP client as a set of tools. It is the same deterministic substrate the CLI drives — the cache, the query engine, the plan-then-apply mutation path, the audit event stream — reachable by an agent whose vault may be remote or off-filesystem.

```sh
norn mcp --cwd /path/to/vault
```

The server speaks JSON-RPC over stdin/stdout. It is **not** a daemon, a network service, or a file watcher: it is a child process the harness launches and keeps alive, one vault per instance, with no background activity between calls.

## Scope (v1)

- **Transport: stdio only.** No HTTP or remote transport.
- **One vault per server.** The vault root is resolved from `-C`/`--cwd` (or the process cwd) at startup and fixed for the server's lifetime. To serve multiple vaults, run multiple servers.
- **Warm cache, per-call freshness.** Config is parsed once at startup; the cache is re-opened on each tool call so every call gets the CLI's per-invocation freshness check without a filesystem watcher.

## Client configuration

Register the server with an MCP client by pointing it at the `norn` binary. For a Claude Code / generic `mcpServers` entry:

```json
{
  "mcpServers": {
    "norn": {
      "command": "norn",
      "args": ["mcp", "--cwd", "/path/to/vault"]
    }
  }
}
```

Add `"--read-only"` to the `args` array for a query-only server (see [Read-only mode](#read-only-mode)).

## Tool catalog

Fourteen tools, split into seven read and seven mutation.

### Read tools

| Tool | What it does |
|---|---|
| `vault.find` | Full-text + metadata document search, with sort, limit, and paging — the same selection surface as `norn find`. |
| `vault.count` | Count documents, total or grouped by a frontmatter field. |
| `vault.get` | Fetch one or more documents: frontmatter, headings, outgoing/incoming/unresolved links, optionally the body. |
| `vault.validate` | Validate graph facts and configured frontmatter/link rules; returns structured findings. |
| `vault.repair_plan` | Produce a deterministic `MigrationPlan` (closest-match link rewrites, frontmatter fixes) **without applying it**. Feed the plan to `vault.apply_plan`. |
| `vault.describe` | Describe the vault for an off-filesystem client — folder tree, declared path rules, frontmatter schema. See [Placing a new document](#placing-a-new-document). |
| `vault.audit` | Read the per-vault mutation audit trail (append-only event stream). Filters: `trace`, `status`, `target`, `since`, `until`, `limit`. Returns flattened event records or raw OTEL objects. Available under `--read-only`. |

### Mutation tools

| Tool | What it does |
|---|---|
| `vault.new` | Create a new document with schema-scaffolded frontmatter from its path. |
| `vault.set` | Update one document's frontmatter (and optionally replace its body), schema-aware. |
| `vault.edit` | Edit one document's body with atomic, content-anchored partial edits (an ordered JSON array of ops, all-or-nothing). |
| `vault.move` | Move/rename a document, cascading backlink rewrites across the vault. |
| `vault.delete` | Delete a document, optionally redirecting incoming links to an alternate target. |
| `vault.rewrite_wikilink` | Retarget every occurrence of a wikilink across the vault (body + frontmatter), without moving any file. |
| `vault.apply_plan` | Apply a `MigrationPlan` (e.g. one returned by `vault.repair_plan`) inline — moves, deletes, link rewrites, frontmatter ops. |

Every mutation tool follows the same safety contract below.

## Mutation-safety contract

**Every mutation tool is dry-run by default.** A call without `confirm: true` runs the full preflight and planning path and returns the planned change — but writes nothing to disk. To apply, pass `confirm: true`:

- A confirmed mutation acquires the **per-vault mutation lock** (the same advisory lock the CLI takes), so an MCP write and a concurrent `norn set` from a shell can't interleave.
- It applies through the **same shared applier** as the CLI — the MCP path and the CLI path can't drift on the mutation semantics.
- It is **audited to the append-only event stream** — the same records `norn set` / `norn migrate` write, carrying a `trace_id` in the tool's response.

This mirrors the CLI's `--dry-run` / apply split: the dry-run is a reviewable forecast, the confirm is the apply step. An agent should inspect the dry-run result before re-issuing with `confirm: true`.

```jsonc
// Dry-run: returns the planned change, writes nothing
{ "name": "vault.set", "arguments": { "target": "notes/task.md", "set": { "status": "backlog" } } }

// Apply: writes the change under the mutation lock, audits the event
{ "name": "vault.set", "arguments": { "target": "notes/task.md", "set": { "status": "backlog" }, "confirm": true } }
```

## Read-only mode

```sh
norn mcp --cwd /path/to/vault --read-only
```

Under `--read-only`, the seven mutation tools are **dropped from `tools/list`** (never registered) and **refused at runtime** if a client calls one anyway (defense in depth). The result is a query-only surface: an agent can find, count, get, validate, plan, and read the audit trail — but cannot write.

## Placing a new document

An MCP client that can't `ls` the vault or read sibling files needs a way to learn where a new document belongs and what frontmatter it should carry. The workflow:

1. **`vault.describe`** — returns the folder tree (so the agent sees where each kind of doc lives), the declared path rules (which path glob gets which frontmatter defaults), and the frontmatter schema (field types, allowed values, required fields).
2. **Construct the path** from the path rules — e.g. a task belongs under `tasks/`, a note under `notes/`.
3. **`vault.new`** — create the document at that path. `vault.new` scaffolds the schema-declared frontmatter for the matching rule, so the new doc starts schema-valid.

Like every mutation tool, `vault.new` is dry-run by default; pass `confirm: true` to write the file.

## Operational notes

- **Build the cache before heavy use.** The server holds the cache warm, but a cold concurrent start can race the first cache rebuild. In-process tool calls are serialized for safety (so concurrent cold-start calls can't collide into "database is locked"), but that means concurrent calls queue behind the first call's rebuild. For heavy use, build the cache first with `norn cache rebuild` (or any prior CLI run against the vault) so the first MCP call is already warm.
- **Await each response before the next call.** Tool calls are serialized in-process but not guaranteed FIFO under request pipelining. A client should await each tool response before issuing the next call — especially for mutations, where ordering matters. This is standard MCP client behavior.

## Known limitations (v1)

These are intentional v1 boundaries, tracked under the ongoing MCP initiative:

- **No prose-body / sub-document edit tool.** `vault.set` replaces a document's whole body; there is no surgical in-body edit yet (that awaits `norn edit`).
- **No HTTP / remote transport.** stdio and local-process only.
- **No declared doc-kind placement templates.** Use `vault.describe` + construct the path rather than naming a doc "kind" and getting a path back.

## See also

- [Agent workflows](agent-workflows.md) — the CLI-side agent contract and loop patterns the MCP tools mirror.
- [Validation and repair](validation.md) — the `MigrationPlan` schema behind `vault.repair_plan` / `vault.apply_plan`.
- [Configuration](configuration.md) — the `.norn/config.yaml` keys `vault.describe` projects.
