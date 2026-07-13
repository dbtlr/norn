---
title: MCP server
description: Run norn as a Model Context Protocol stdio server — the 14-tool catalog, the dry-run/confirm mutation contract, the document-placement workflow, and the warm-cache and call-ordering notes.
---

# MCP server

`norn mcp` runs a [Model Context Protocol](https://modelcontextprotocol.io/) server over stdio, exposing the vault to any MCP client as a set of tools. It is the same deterministic substrate the CLI drives — the cache, the query engine, the plan-then-apply mutation path, the audit event stream — reachable by an agent whose vault may be remote or off-filesystem.

```sh
norn mcp --cwd /path/to/vault
```

The server speaks JSON-RPC over stdin/stdout. It is **not** a daemon, a network service, or a file watcher: it is a child process the harness launches and keeps alive, one vault per instance, with no background activity between calls. For a persistent, multi-vault alternative that stays warm across calls, see [`norn serve`](service.md).

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

## Tool catalog

Fourteen tools, split into seven read and seven mutation.

### Read tools

| Tool | What it does |
|---|---|
| `vault.find` | Full-text + metadata document search, with sort, limit, and paging — the same selection surface as `norn find`. |
| `vault.count` | Count documents, total or grouped by a frontmatter field. |
| `vault.get` | Fetch one or more documents: frontmatter, headings, outgoing/incoming/unresolved links, optionally the body. |
| `vault.validate` | Validate graph facts and configured frontmatter/link rules; returns structured findings. |
| `vault.repair` | Produce a deterministic `MigrationPlan` (closest-match link rewrites, frontmatter fixes) **without applying it**. Feed the plan to `vault.apply`. |
| `vault.describe` | Describe the vault for an off-filesystem client — folder tree, declared path rules, creatable rules, inbox, frontmatter schema. See [Placing a new document](#placing-a-new-document). |
| `vault.audit` | Read the per-vault mutation audit trail (append-only event stream). Filters: `trace`, `status`, `target`, `since`, `until`, `limit`. Returns flattened event records or raw OTEL objects. |

### Mutation tools

| Tool | What it does |
|---|---|
| `vault.new` | Create a new document in three modes: explicit `path`, rule-targeted (`rule`/`title`/`vars`), or inbox fallback (`title` only). |
| `vault.set` | Update one document's frontmatter (and optionally replace its body), schema-aware. |
| `vault.edit` | Edit one document's body with atomic, content-anchored partial edits (an ordered JSON array of ops, all-or-nothing). |
| `vault.move` | Move/rename a document, cascading backlink rewrites across the vault. |
| `vault.delete` | Delete a document, optionally redirecting incoming links to an alternate target. |
| `vault.rewrite_wikilink` | Retarget every occurrence of a wikilink across the vault (body + frontmatter), without moving any file. |
| `vault.apply` | Apply a `MigrationPlan` (e.g. one returned by `vault.repair`) inline — moves, deletes, link rewrites, frontmatter ops. |

Every mutation tool follows the same safety contract below.

## Mutation-safety contract

**Every mutation tool is dry-run by default.** A call without `confirm: true` runs the full preflight and planning path and returns the planned change — but writes nothing to disk. To apply, pass `confirm: true`:

- A confirmed mutation acquires the **per-vault mutation lock** (the same advisory lock the CLI takes), so an MCP write and a concurrent `norn set` from a shell can't interleave.
- It applies through the **same shared applier** as the CLI — the MCP path and the CLI path can't drift on the mutation semantics.
- It is **audited to the append-only event stream** — the same records `norn set` / `norn apply` write, carrying a `trace_id` in the tool's response.

This mirrors the CLI's `--dry-run` / apply split: the dry-run is a reviewable forecast, the confirm is the apply step. An agent should inspect the dry-run result before re-issuing with `confirm: true`.

```jsonc
// Dry-run: returns the planned change, writes nothing
{ "name": "vault.set", "arguments": { "target": "notes/task.md", "field_json": ["status=\"backlog\""] } }

// Apply: writes the change under the mutation lock, audits the event
{ "name": "vault.set", "arguments": { "target": "notes/task.md", "field_json": ["status=\"backlog\""], "confirm": true } }
```

`vault.set` parameters:

| Parameter | Type | Description |
|---|---|---|
| `target` | string | Target document (stem or path), as `norn set` accepts. |
| `field_json` | string[] (optional) | Frontmatter fields to set, as ordered `KEY=JSON` tokens. Applied in order and fed through `norn set --field-json KEY=JSON` — coerced and schema-validated exactly as the CLI flag is. A key repeated across tokens accumulates into an array. |
| `field` | string[] (optional) | Frontmatter field overrides in `KEY=VALUE` format — the coercing counterpart to `field_json` (string coercion against the schema). |
| `push` | string[] (optional) | Append to a list-typed frontmatter field, as ordered `KEY=VALUE` tokens. A key repeated across tokens pushes each value in turn. Creates a single-element array if the key does not exist. |
| `pop` | string[] (optional) | Remove from a list-typed frontmatter field, as ordered `KEY=VALUE` tokens. Silent no-op for a member that is not present. |
| `remove` | string[] (optional) | Frontmatter keys to remove entirely. Silent no-op for missing keys. |
| `body` | string (optional) | Wholesale body replacement — the MCP analogue of `norn set --body-from-stdin`. Absent = body unchanged. |
| `force` | bool (optional) | Bypass schema enforcement (type validation + required-field protection). |
| `confirm` | bool (default `false`) | `false` = dry-run (returns plan, writes nothing); `true` = apply. |

## Placing a new document

An MCP client that can't `ls` the vault or read sibling files needs a way to learn where a new document belongs and what frontmatter it should carry. Two workflows:

### Path-construction workflow (manual path)

1. **`vault.describe`** — returns the folder tree (`folders`), the declared path rules (`path_rules`: which path glob gets which frontmatter defaults), and the frontmatter schema. Also returns `creatable_rules` and `inbox` (see below).
2. **Construct the path** from the path rules — e.g. a task belongs under `tasks/`, a note under `notes/`.
3. **`vault.new { path: "...", confirm: true }`** — create the document at that path.

### Rule-targeted workflow (recommended for off-filesystem agents)

When the vault declares creatable rules, the agent can skip path construction entirely:

1. **`vault.describe`** — inspect `creatable_rules`. Each entry carries `name` (the rule handle), `target` (the path template), `required_vars` (variable names the template needs), `frontmatter_defaults`, and `body` (optional body scaffold).
2. **`vault.new { rule: "task", title: "…", vars: { "workspace": "norn" }, confirm: true }`** — norn derives the concrete path from the rule's `target` template, applies `frontmatter_defaults`, and seeds the body from the `body` scaffold. No path guessing needed.

If `vault.describe` returns a non-null `inbox`, an agent can also use **`vault.new { title: "…", confirm: true }`** (no `path`, no `rule`) to place a document in the inbox as `<inbox>/<title|slugify>.md`.

`vault.new` parameters:

| Parameter | Type | Description |
|---|---|---|
| `path` | string (optional) | Vault-relative path. Mutually exclusive with `rule`. |
| `rule` | string (optional) | Name of a creatable rule (from `vault.describe` `creatable_rules`). Mutually exclusive with `path`. |
| `title` | string (optional) | Document title; fills `{{title}}` in templates. Required when the target template references `{{title}}` and for inbox mode. |
| `vars` | object (optional) | Template variable bag; keys fill `{{var.KEY}}` in the rule's target and body templates. Supply every name listed in `required_vars`. |
| `field` | string[] (optional) | Frontmatter overrides in `KEY=VALUE` format. |
| `field_json` | string[] (optional) | Frontmatter overrides in `KEY=JSON` format. |
| `body` | string (optional) | Explicit body content; takes precedence over the rule's body scaffold. |
| `parents` | bool (optional) | Auto-create missing parent directories. |
| `force` | bool (optional) | Overwrite an existing file. |
| `confirm` | bool (default `false`) | `false` = dry-run (returns plan, writes nothing); `true` = apply. |

`vault.describe` output includes two new fields alongside `folders`, `path_rules`, and `schema`:

| Field | Type | Description |
|---|---|---|
| `creatable_rules` | array | Rules that support `vault.new { rule: "…" }`. Each: `name`, `target`, `required_vars`, `frontmatter_defaults`, `body`. |
| `inbox` | string or null | The configured `inbox.path`, if any. When non-null, `vault.new { title: "…" }` routes the doc there. |

Like every mutation tool, `vault.new` is dry-run by default; pass `confirm: true` to write the file.

## Operational notes

- **Warm reads run concurrently; stdio cold calls stay serialized.** Under the warm daemon (`norn serve`), read tool calls verified fresh at their request boundary run in parallel over a per-generation pool of read-only connections; stale reads coalesce onto one shared refresh, and reads during a daemon-side write serve the last committed WAL snapshot. The stdio server (`norn mcp`) opens a fresh cache per call and still serializes calls in-process (concurrent cold-start calls can't collide into "database is locked"), so concurrent stdio calls queue behind the first call's rebuild. For heavy stdio use, build the cache first with `norn cache rebuild` (or any prior CLI run against the vault) so the first MCP call is already warm.
- **Await each mutation's response before the next call.** Mutations serialize on the per-vault mutation lock and their ordering matters — a client should await each mutation's response before issuing the next. Reads may be pipelined freely against the warm daemon; each is independently freshness-verified at its own request boundary.

## Known limitations (v1)

These are intentional v1 boundaries, tracked under the ongoing MCP initiative:

- **No prose-body / sub-document edit tool.** `vault.set` replaces a document's whole body; there is no surgical in-body edit yet (that awaits `norn edit`).
- **No HTTP / remote transport.** stdio and local-process only.

## See also

- [Warm host daemon](service.md) — `norn serve`, the persistent multi-vault alternative to this stdio server.
- [Agent workflows](agent-workflows.md) — the CLI-side agent contract and loop patterns the MCP tools mirror.
- [Validation and repair](validation.md) — the `MigrationPlan` schema behind `vault.repair` / `vault.apply`.
- [Configuration](configuration.md) — the `.norn/config.yaml` keys `vault.describe` projects.
