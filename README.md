# norn

**Your Markdown vault: deterministic, structured, yours. For terminals and agents.**

Norn gives you your Markdown vault as a deterministic graph — one you can query, validate, and repair from the command line. Built for shells, scripts, and coding agents.

## Why Norn?

Obsidian gives you a GUI over your Markdown vault. Norn gives you the same vault as a deterministic graph — documents, links, headings, frontmatter — that humans and agents can both work with from the command line.

**Built for humans and agents to share a vault.** Keeping a vault organized is hard. Humans skip metadata when they're in flow. Agents drift the conventions when each write is a local decision with no global view. And any non-trivial question — *what's new today, what violates this convention, where does this belong?* — costs an agent dozens of tool calls and still gets the answer wrong.

Norn is the deterministic layer underneath. Humans write freely; agents handle the maintenance. One call to query the graph, one call to find every drift, one call to plan a migration. The agent decides; Norn enumerates.

**Keep Obsidian's superpowers without the lock-in.** Note renames, frontmatter-driven views, graph navigation — these are the features that make Obsidian feel powerful, and they're the reason it's hard to leave. Norn implements them headlessly, against a deterministic graph: rename a note and an agent can rewrite every reference correctly, query notes the way Obsidian's bases would, and trace the link graph from the command line. Use Obsidian if you like it. Use something else if you don't. Your vault, and its superpowers, come with you either way.

**Query your vault like a database.** Filter notes by frontmatter (`norn find --eq type:task --eq status:backlog`), trace backlinks, find unresolved links, search across documents. Records output for your eyes, JSON for your scripts and agents. A cron-driven agent can triage your new notes every morning with zero ambiguity about what's new.

**Frontmatter is a contract, not decoration.** Declare what every `type=task` note must look like: required fields, allowed values, expected shapes. Norn enforces it. When you (or an agent) ask for all backlog tasks, you can trust the answer — because `status` is guaranteed to exist and `status=backlog` means what you defined it to mean. Most note tools read frontmatter; Norn treats it as the schema your vault answers questions against.

**Declare your standards. Find the drift.** Write rules in `.norn/config.yaml` for the conventions your vault should follow — or have your agent do it for you. Required frontmatter fields, link shapes, file locations. Norn finds every violation in one pass, with stable codes you can filter and triage.

**Repair deterministically.** Two steps, always: produce a JSON repair plan, then apply it. Plans are schema-versioned, inspectable, and idempotent. An agent can propose a migration; you (or another agent) review the plan before it touches disk. No LLMs rewriting your notes behind your back.

**One contract, humans and agents.** Stable JSON and JSONL output, filterable everywhere — the same shape whether you're piping to `jq`, building an alias, or feeding a coding agent.

**What it is:**

- A deterministic graph of your vault's documents, links, and frontmatter.
- Configurable validation, driven by `validate.rules` in `.norn/config.yaml`.
- A plan-then-apply repair loop with schema-versioned JSON plans.
- Stable JSON and JSONL contracts so the same output drives humans, scripts, and agents.
- **A CLI _and_ an MCP server**, both backed by the same deterministic primitives. Run `norn <command>` from a shell, or `norn mcp` to expose the vault to a Model Context Protocol client as a set of tools (see [MCP server](#mcp-server) below). Norn is not a daemon, sync service, or file watcher — the MCP server is a stdio process you launch per harness, holding one vault per instance; there is no background process and no network sync.

## Install

The fastest path is the hosted shell installer:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/dbtlr/norn/releases/latest/download/norn-run-installer.sh \
  | sh
```

Build from source with Cargo:

```sh
cargo install --git https://github.com/dbtlr/norn norn-run
```

For manual binary downloads, the safer download-then-run installer form, and verification steps, see [docs/installation.md](docs/installation.md).

## Quick start

Clone the repo, then run three commands against the bundled fixture vault:

```sh
norn -C fixtures/basic find --all --format records
norn -C fixtures/basic validate --summary
norn -C fixtures/basic validate --code 'link-*' --format jsonl | head
```

You should see a small inventory of Markdown documents, a finding summary that includes a handful of intentional drift cases, and a JSONL stream of unresolved links from the fixture.

Run the same shape against your own vault:

```sh
norn -C /path/to/vault validate --summary
norn -C /path/to/vault find --all --format paths | head
```

For a deeper walkthrough including scoped rules and a first repair plan, see [docs/quickstart.md](docs/quickstart.md).

## Core workflows

| Workflow | Command shape | Docs |
| --- | --- | --- |
| Inventory documents | `norn find --all --format records` | [find](docs/commands/find.md) |
| Find | `norn find --text "..." --eq k:v` | [find](docs/commands/find.md) |
| Count / group | `norn count --by status` | [count](docs/commands/count.md) |
| Inspect one document | `norn get <path-or-stem>` | [get](docs/commands/get.md) |
| Walk unresolved links | `norn validate --code 'link-*'` | [validate](docs/commands/validate.md) |
| Validate against rules | `norn validate --summary` | [validate](docs/commands/validate.md) |
| Plan a repair | `norn repair --plan --out plan.json` | [repair](docs/commands/repair.md) |
| Apply a repair | `norn migrate plan.json` | [migrate](docs/commands/migrate.md) |
| Create a document | `norn new <path>` | [new](docs/commands/new.md) |
| Update frontmatter | `norn set <doc> --field k=v` | [set](docs/commands/set.md) |
| Move a document | `norn move <src> <dst>` | [move](docs/commands/move.md) |
| Delete a document | `norn delete <doc>` | [delete](docs/commands/delete.md) |

Commands accept `--format json|jsonl` (stable contracts) plus format-specific human-readable options (`records`, `text`, `paths`). JSON and JSONL contracts are stable across point releases; human-readable formats may evolve.

## For agents and automation

Norn is designed to be a first-class tool for coding agents:

- **Stable contracts.** JSON for one-shot dispatch, JSONL for streaming queues, and a schema-versioned repair plan (`schema_version: 9`).
- **Plan/apply boundary.** Mutation is always two steps: produce a plan artifact, then apply it. Apply rejects mismatched vault roots, stale document hashes, and unsupported schema versions.
- **Filterable triage.** `--code`, `--severity`, `--field`, `--rule`, `--path`, `--target`, `--reason` apply to both raw output and `--summary`, so an agent can size a queue before reading it.
- **Vault targeting.** Use `-C <path>` (or `--cwd <path>`) to point `norn` at a specific vault root, or omit it to run against the current directory.

For the agent-facing contract, start at [docs/agent-workflows.md](docs/agent-workflows.md). To install the agent skill into your coding agent of choice, see [integrations/agent-skill/README.md](integrations/agent-skill/README.md).

## MCP server

`norn mcp` runs a [Model Context Protocol](https://modelcontextprotocol.io/) server over stdio, exposing the same deterministic primitives as the CLI to any MCP client — a query-and-mutate surface for an agent whose vault may be remote or off-filesystem.

```sh
norn mcp --cwd /path/to/vault
```

It serves 12 tools: six read (`vault.find`, `vault.count`, `vault.get`, `vault.validate`, `vault.repair_plan`, `vault.describe`) and six mutation (`vault.new`, `vault.set`, `vault.move`, `vault.delete`, `vault.rewrite_wikilink`, `vault.apply_plan`). Every mutation tool is **dry-run by default** — it returns the planned change and writes nothing until you pass `confirm: true`, at which point the change is applied under the per-vault mutation lock and audited to the same append-only event stream as the CLI. Pass `--read-only` to drop the six mutation tools entirely for a query-only server.

Register it with an MCP client by pointing the client's server config at the binary. For a Claude Code / generic `mcpServers` entry:

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

One server instance serves one vault. See [docs/mcp-server.md](docs/mcp-server.md) for the full tool catalog, the document-placement workflow (`vault.describe` → construct path → `vault.new`), and the warm-cache and call-ordering notes.

## Documentation

| Topic | Page |
| --- | --- |
| Install | [docs/installation.md](docs/installation.md) |
| Quick start | [docs/quickstart.md](docs/quickstart.md) |
| Concepts (graph, links, validation) | [docs/concepts.md](docs/concepts.md) |
| Command reference | [docs/commands.md](docs/commands.md) |
| Configuration (`.norn/config.yaml`) | [docs/configuration.md](docs/configuration.md) |
| Validation and repair | [docs/validation.md](docs/validation.md) |
| Validate rule shape (selectors + constraints) | [docs/rule-shape.md](docs/rule-shape.md) |
| Agent workflows | [docs/agent-workflows.md](docs/agent-workflows.md) |
| MCP server | [docs/mcp-server.md](docs/mcp-server.md) |
| Warm host daemon (`norn serve`) | [docs/service.md](docs/service.md) |
| Development | [docs/development.md](docs/development.md) |
| Releases and versioning | [docs/releases.md](docs/releases.md) |

Worked examples live under [examples/README.md](examples/README.md).

## Project status

Norn is pre-1.0. Minor releases may include breaking changes to CLI flags, config keys, and JSON contracts. Breaking changes are called out in [CHANGELOG.md](CHANGELOG.md) with migration notes.

The current focus is the deterministic drift-healing loop (validate → plan → apply → verify). Severity escalation, output schema versioning envelopes, and link/path apply are tracked for upcoming releases.

## Development

Set up the toolchain with [mise](https://mise.jdx.dev/) and use [just](https://github.com/casey/just) for common recipes:

```sh
mise install
mise exec -- just build
mise exec -- just test
mise exec -- just verify
```

For the full developer workflow including MSRV policy and release process, see [docs/development.md](docs/development.md) and [docs/releases.md](docs/releases.md).

Contributions welcome — see [CONTRIBUTING.md](CONTRIBUTING.md) and [SECURITY.md](SECURITY.md).

## License

MIT. See [LICENSE](LICENSE).
