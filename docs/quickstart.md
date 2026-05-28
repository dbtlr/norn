---
title: Quick start
description: First-run walkthrough against the bundled fixture vault, then the same shape against a real Markdown vault on disk.
---

# Quick start

Two paths: a 60-second loop against the bundled fixture, then the same shape against a real vault you own.

## Against the bundled fixture

Clone the repo and run from the workspace root:

```bash
git clone https://github.com/dbtlr/norn
cd norn
```

List the fixture's documents:

```bash
norn -C fixtures/basic find --all --format records
```

You should see a small inventory: `alpha.md`, `beta.md`, `broken-frontmatter.md`, `duplicate.md`, plus a few items under `folder/` and `other/`.

Walk unresolved links:

```bash
norn -C fixtures/basic validate --code 'link-*' --format jsonl | head
```

The fixture intentionally includes a `[[missing]]` wikilink, an ambiguous `[[duplicate]]` reference, and missing-anchor cases — these surface here as JSONL rows.

Run validation against the fixture's default config (`fixtures/basic/.norn/config.yaml` if present, otherwise built-in defaults):

```bash
norn -C fixtures/basic validate --summary --format records
```

You'll see grouped finding counts: unresolved-link counts, ambiguous-link counts, and any rule violations the fixture exercises.

## Against your own vault

Pick a Markdown vault you own — an Obsidian vault, a notes directory, a docs site source — and run:

```bash
norn -C /path/to/vault find --all --format paths | head
norn -C /path/to/vault validate --summary --format records
```

Out of the box `norn` parses Obsidian-compatible internal links: body wikilinks, embeds, frontmatter wikilinks, URL-decoded Markdown links, extensionless Markdown note links, heading anchors, and block references.

## Targeting a vault

Pass `-C <path>` (alias `--cwd`) to run any command against an arbitrary vault directory. When `-C` is omitted, `norn` runs against the current directory. Either way, `.norn/config.yaml` is discovered at the effective root if it exists.

## A first config

Add a minimal `.norn/config.yaml` at the root of your vault to declare what to ignore:

```yaml
files:
  ignore:
    - "node_modules/**"
    - ".obsidian/**"
    - "target/**"
```

Run `norn validate --summary` again — you'll see the file inventory shrink and the ignored items drop out of unresolved-link reports.

For a typed-notes config with `validate.rules`, see [examples/config-typed-notes.yaml](../examples/config-typed-notes.yaml) and the [configuration guide](configuration.md).

## A first repair

If validation surfaces a deterministic drift case (e.g., a disallowed `status` value), write a plan:

```bash
norn -C /path/to/vault repair --plan --out plan.json
cat plan.json | head -40
```

Inspect the plan. The `changes` array is what apply will write; `skipped_findings` is what couldn't be planned deterministically. Dry-run, then apply with verification:

```bash
norn -C /path/to/vault migrate plan.json --dry-run --format json
norn -C /path/to/vault migrate plan.json --verify --format json
```

For the full repair workflow including snapshot tags and live maintenance recipes, see [validation.md](validation.md).

## Next steps

- [Concepts](concepts.md) — the graph, link model, validation/repair distinction.
- [Commands](commands.md) — one-line reference for every subcommand.
- [Configuration](configuration.md) — `.norn/config.yaml` schema.
- [Validation and repair](validation.md) — finding codes, filters, recipes.
- [Agent workflows](agent-workflows.md) — using `norn` from a coding agent.
