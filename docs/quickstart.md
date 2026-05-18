---
title: Quick start
description: First-run walkthrough against the bundled fixture vault, then the same shape against a real Markdown vault on disk.
---

# Quick start

Two paths: a 60-second loop against the bundled fixture, then the same shape against a real vault you own.

## Against the bundled fixture

Clone the repo and run from the workspace root:

```bash
git clone https://github.com/dbtlr/vault-cli
cd vault-cli
```

List the fixture's documents:

```bash
vault -C fixtures/basic docs list --format table
```

You should see a small inventory: `alpha.md`, `beta.md`, `broken-frontmatter.md`, `duplicate.md`, plus a few items under `folder/` and `other/`.

Walk unresolved links:

```bash
vault -C fixtures/basic links unresolved --format jsonl | head
```

The fixture intentionally includes a `[[missing]]` wikilink, an ambiguous `[[duplicate]]` reference, and missing-anchor cases — these surface here as JSONL rows.

Run validation against the fixture's default config (`fixtures/basic/.vault/config.yaml` if present, otherwise built-in defaults):

```bash
vault -C fixtures/basic validate --summary --format table
```

You'll see grouped finding counts: unresolved-link counts, ambiguous-link counts, and any rule violations the fixture exercises.

## Against your own vault

Pick a Markdown vault you own — an Obsidian vault, a notes directory, a docs site source — and run:

```bash
vault -C /path/to/vault docs list --format paths | head
vault -C /path/to/vault validate --summary --format table
```

Out of the box `vault` parses Obsidian-compatible internal links: body wikilinks, embeds, frontmatter wikilinks, URL-decoded Markdown links, extensionless Markdown note links, heading anchors, and block references.

## Register a vault for repeated use

If you'll run commands against the same vault often, register it once:

```bash
vault registry add myvault /path/to/vault
vault registry list --format table
```

Now you can target it with `--vault <name>` instead of `-C`:

```bash
vault --vault myvault validate --summary --format table
vault --vault myvault docs list --filter status:draft --format paths
```

Registry state lives at `$XDG_CONFIG_HOME/vault/registry.yaml` (or `~/.config/vault/registry.yaml` if `XDG_CONFIG_HOME` is unset). `--vault` and `-C` are mutually exclusive.

## A first config

Add a minimal `.vault/config.yaml` at the root of your vault to declare what to ignore:

```yaml
files:
  ignore:
    - "node_modules/**"
    - ".obsidian/**"
    - "target/**"
```

Run `vault validate --summary` again — you'll see the file inventory shrink and the ignored items drop out of unresolved-link reports.

For a typed-notes config with `validate.rules`, see [examples/config-typed-notes.yaml](../examples/config-typed-notes.yaml) and the [configuration guide](configuration.md).

## A first repair

If validation surfaces a deterministic drift case (e.g., a disallowed `status` value), write a plan:

```bash
vault -C /path/to/vault repair plan --out repair.json
cat repair.json | head -40
```

Inspect the plan. The `changes` array is what apply will write; `skipped_findings` is what couldn't be planned deterministically. Dry-run, then apply with verification:

```bash
vault -C /path/to/vault repair apply repair.json --dry-run --format json
vault -C /path/to/vault repair apply repair.json --verify --format json
```

For the full repair workflow including snapshot tags and live maintenance recipes, see [validation.md](validation.md).

## Next steps

- [Concepts](concepts.md) — the graph, link model, validation/repair distinction.
- [Commands](commands.md) — one-line reference for every subcommand.
- [Configuration](configuration.md) — `.vault/config.yaml` schema.
- [Validation and repair](validation.md) — finding codes, filters, recipes.
- [Agent workflows](agent-workflows.md) — using `vault` from a coding agent.
