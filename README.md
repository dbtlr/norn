# vault

Deterministic Markdown vault graph, link, and validation tooling.

> `vault` gives humans and agents the same deterministic view of a Markdown vault: documents, links, frontmatter, validation findings, and machine-readable drift reports.

The current binary name is `vault`. The crate is `vault-cli`.

## Why vault?

`vault` is a CLI for inspecting and healing drift in a Markdown vault you own. It walks your files, builds a graph (documents, headings, links, attachments), and lets you assert standards against it with a `validate -> plan -> apply -> verify` loop. The graph and validation passes are read-only; repair is explicit and produces inspectable JSON artifacts that you (or an agent) review before applying.

What it is:

- A deterministic graph of your vault's documents, links, and frontmatter.
- A configurable validation layer (`validate.rules` in `.vault/config.yaml`).
- A planned, applyable repair layer with schema-versioned JSON plans.
- Stable JSON and JSONL contracts so the same output drives humans, scripts, and agents.

What it isn't:

- A LLM-backed rewriter. Repairs are deterministic transformations declared in config.
- A semantic search engine. Search is literal substring + frontmatter + path glob.
- A daemon, server, or sync tool. It runs once per invocation against a vault on disk.

## Install

The fastest path is the hosted shell installer:

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/dbtlr/vault-cli/releases/latest/download/vault-cli-installer.sh \
  | sh
```

Build from source with Cargo:

```bash
cargo install --git https://github.com/dbtlr/vault-cli vault-cli
```

For manual binary downloads, the safer download-then-run installer form, and verification steps, see [docs/installation.md](docs/installation.md).

## Quick start

Clone the repo, then run three commands against the bundled fixture vault:

```bash
vault -C fixtures/basic docs list --format table
vault -C fixtures/basic validate --summary
vault -C fixtures/basic links unresolved --format jsonl | head
```

You should see a small inventory of Markdown documents, a finding summary that includes a handful of intentional drift cases, and a JSONL stream of unresolved links from the fixture.

Run the same shape against your own vault:

```bash
vault -C /path/to/vault validate --summary
vault -C /path/to/vault docs list --format paths | head
```

For a deeper walkthrough including registry-targeted vaults, scoped rules, and a first repair plan, see [docs/quickstart.md](docs/quickstart.md).

## Core workflows

| Workflow | Command shape | Docs |
|---|---|---|
| Inventory documents | `vault docs list --format table` | [commands.md](docs/commands.md) |
| Inspect one document | `vault docs inspect <path-or-stem>` | [commands.md](docs/commands.md) |
| Walk links | `vault links list / unresolved / backlinks` | [commands.md](docs/commands.md) |
| Validate against rules | `vault validate --summary` | [validation.md](docs/validation.md) |
| Plan a repair | `vault repair plan --out repair.json` | [validation.md](docs/validation.md) |
| Apply a repair | `vault repair apply repair.json --verify` | [validation.md](docs/validation.md) |
| Plan link/path moves | `vault repair links --target <path>` | [validation.md](docs/validation.md) |
| Search | `vault search --text "..." --filter k:v` | [commands.md](docs/commands.md) |

Every command accepts `--format table|json|jsonl|paths` where applicable. JSON and JSONL contracts are stable across point releases; table output is for humans and may evolve.

## For agents and automation

`vault` is designed to be a first-class tool for coding agents:

- **Stable contracts.** JSON for one-shot dispatch, JSONL for streaming queues, and a schema-versioned repair plan (`schema_version: 3`).
- **Plan/apply boundary.** Mutation is always two steps: produce a plan artifact, then apply it. Apply rejects mismatched vault roots, stale document hashes, and unsupported schema versions.
- **Filterable triage.** `--code`, `--severity`, `--field`, `--rule`, `--path`, `--target`, `--reason` apply to both raw output and `--summary`, so an agent can size a queue before reading it.
- **Vault targeting.** Use `-C <path>` for ad-hoc invocations or `vault registry add <name> <path>` once and target with `--vault <name>`.

For the agent-facing contract, start at [docs/agent-workflows.md](docs/agent-workflows.md). To install the agent skill into your coding agent of choice, see [integrations/agent-skill/README.md](integrations/agent-skill/README.md).

## Documentation

| Topic | Page |
|---|---|
| Install | [docs/installation.md](docs/installation.md) |
| Quick start | [docs/quickstart.md](docs/quickstart.md) |
| Concepts (graph, links, validation) | [docs/concepts.md](docs/concepts.md) |
| Command reference | [docs/commands.md](docs/commands.md) |
| Configuration (`.vault/config.yaml`) | [docs/configuration.md](docs/configuration.md) |
| Validation and repair | [docs/validation.md](docs/validation.md) |
| Validate rule shape (selectors + constraints) | [docs/rule-shape.md](docs/rule-shape.md) |
| Agent workflows | [docs/agent-workflows.md](docs/agent-workflows.md) |
| Development | [docs/development.md](docs/development.md) |
| Releases and versioning | [docs/releases.md](docs/releases.md) |

Worked examples live under [examples/README.md](examples/README.md).

## Project status

`vault` is pre-1.0. Minor releases may include breaking changes to CLI flags, config keys, and JSON contracts. Breaking changes are called out in [CHANGELOG.md](CHANGELOG.md) with migration notes.

The current focus is the deterministic drift-healing loop (validate -> plan -> apply -> verify). Severity escalation, output schema versioning envelopes, and link/path apply are tracked for v0.27 and beyond.

## Development

Set up the toolchain with [mise](https://mise.jdx.dev/) and use [just](https://github.com/casey/just) for common recipes:

```bash
mise install
mise exec -- just build
mise exec -- just test
mise exec -- just verify
```

For the full developer workflow including MSRV policy and release process, see [docs/development.md](docs/development.md) and [docs/releases.md](docs/releases.md).

Contributions welcome — see [CONTRIBUTING.md](CONTRIBUTING.md) and [SECURITY.md](SECURITY.md).

## License

MIT. See [LICENSE](LICENSE).
