---
title: Agent workflows
description: Stable JSON and JSONL contracts, agent loop patterns, and common harness gotchas for driving vault from a coding agent.
---

# Agent workflows

`vault` is designed to be a first-class tool for coding agents. This page documents the contracts an agent can rely on, recommended loop patterns, and common pitfalls.

## Stable contracts

| Contract | Surface | Stability |
|---|---|---|
| JSON output | `--format json` on every command | Stable across point releases; breaking changes called out in CHANGELOG. |
| JSONL output | `--format jsonl` on every command | Same. |
| Paths output | `--format paths` on commands that emit per-row paths | Stable; one unique vault-relative path per row. |
| Repair plan schema | `repair plan` JSON artifact | Schema-versioned (`schema_version` field). Apply rejects mismatched versions. |
| Apply report schema | `repair apply` JSON output | Stable across the matching plan schema version. |
| Finding codes | `vault validate` output `code` field | Stable; renames are breaking changes called out in CHANGELOG. |

Table output is for humans and may evolve between point releases. Agents should always pass an explicit `--format json` or `--format jsonl`.

## Vault targeting

An agent should detect the vault root before running any command. The two ways:

1. **`-C <path>` (alias `--cwd`).** One-shot invocation against an arbitrary directory.
   ```bash
   vault -C /path/to/vault validate --summary --format json
   ```
2. **Process cwd.** When `-C` is not set, `vault` runs against the current directory. Discovery of `.vault/config.yaml` is implicit.

`--cwd PATH` is the only vault-targeting mechanism. An agent operating on multiple vaults should pass `-C` per command.

## Recommended agent loop

For a typical drift-healing task:

1. **Detect.** `vault validate --summary --format json` — get a finding shape before reading individuals.
2. **Triage.** Filter by `--code`, `--field`, `--rule`, `--path` to scope the queue. Re-run `--summary` to confirm the filter's size.
3. **Plan.** `vault repair plan --out repair.json` (with the same filters). Read the plan's `changes` and `skipped_findings`.
4. **Review.** Confirm `changes` are intended; surface `skipped_findings` to the human or follow `next_actions`.
5. **Dry-run.** `vault repair apply repair.json --dry-run --format json` — confirms the plan is applyable without writing. (Or pipe directly: `vault repair plan --format json | vault repair apply --dry-run --format json`.)
6. **Apply.** `vault repair apply repair.json --verify --format json` — writes and re-validates.
7. **Verify.** Inspect the apply report's `plan_context` and the post-apply validation summary.

For a read-only inspection task (no mutation):

1. `vault find --all --format json` or `vault count --by <field> --format json`.
2. `vault validate --summary --format json` to spot drift.
3. `vault get <target> --format json` for one-document detail.

## Read-only commands

These commands never write to the vault. An agent can run them with confidence:

- `vault find`
- `vault count`
- `vault get`
- `vault validate` (with or without `--summary`, with or without filters)
- `vault repair plan` (produces an artifact; does not modify the vault)

`vault set`, `vault move`, and `vault delete` are mutation commands; pass `--dry-run` to preview without writing. Only `vault repair apply`, `vault set`, `vault move`, and `vault delete` (without `--dry-run`) write to the vault. The repair plan is provided via a positional file path, via `-`, or via stdin (the pipeline form `vault repair plan --format json | vault repair apply` composes plan + apply in one shot).

## Output sketches

### Validation summary (JSON)

```json
{
  "total": 12,
  "codes": { "frontmatter-required-field-missing": 5, "link-target-missing": 7 },
  "severities": { "warning": 12 },
  "rules": { "typed-note": 5 },
  "fields": { "kind": 5 },
  "paths": { "notes": 8, "tasks": 4 }
}
```

### Validation finding (JSONL row)

```json
{"code":"frontmatter-disallowed-value","severity":"warning","path":"tasks/triage.md","rule":"task-status","field":"status","actual_value":"someday","allowed_values":["backlog","in_progress","completed","wont_do"]}
```

### Repair plan (JSON)

```json
{
  "schema_version": 8,
  "vault_root": "/abs/path/to/vault",
  "source_filters": { "code": "frontmatter-disallowed-value", "field": "status" },
  "summary": {
    "findings": 4,
    "planned_changes": 3,
    "skipped": { "by_reason": { "no-rule-matched": 1 }, "total": 1 }
  },
  "changes": [ /* ... */ ],
  "skipped_findings": [ /* with skip_reason + reason_code */ ]
}
```

### Apply report (JSON)

```json
{
  "applied": 3,
  "files_changed": 3,
  "verify": { "total": 0 },
  "plan_context": {
    "skipped": { "by_reason": { "no-rule-matched": 1 }, "total": 1 }
  }
}
```

## Filter-based triage

The filter dimensions on `vault validate` (`--code`, `--severity`, `--field`, `--rule`, `--path`, `--target`, `--reason`) are designed for agent-driven triage. Comma-separated values within one filter are ORed; different filters are ANDed.

Use `--summary` first to size a queue, then re-run without `--summary` to read the queue itself:

```bash
vault validate --code frontmatter-invalid-type --field modified --summary --format json
vault validate --code frontmatter-invalid-type --field modified --format jsonl
```

## Plan/apply boundary

Two rules an agent must follow:

1. **Use the appropriate write surface.** For operator-driven one-doc mutations, `vault set` (frontmatter + body), `vault move`, and `vault delete` are the CRUD surface. For finding-driven batch repairs, `vault repair apply` is the only path — it consumes a plan artifact and applies deterministic changes with precondition checks. Never edit vault files directly; the graph state would diverge from the cache.
2. **Always pass the plan that matches the current vault state.** Apply checks document hashes; if a file changed since the plan was created, the change is rejected for that file. Re-plan rather than re-apply with `--force` (there is no `--force` for repair apply).

`--dry-run` confirms the plan is applyable without writing. `--verify` runs validation after apply and includes the result in the report.

## Using vault set for targeted frontmatter updates

When `vault validate` surfaces drift on a single document, `vault set` is the natural follow-up
for a targeted fix without a full plan/apply cycle:

```bash
# 1. Validate — surface a disallowed-value finding on one doc
vault validate --code frontmatter-disallowed-value --field status --format jsonl

# 2. Fix — update that document's status field directly
vault set notes/task.md --field status=backlog --dry-run
vault set notes/task.md --field status=backlog --yes

# 3. Re-validate — confirm the finding is gone
vault validate --code frontmatter-disallowed-value --field status --format jsonl
```

For batch fixes across many documents, prefer the `repair plan` → `repair apply` loop.
`vault set` is best for targeted one-doc mutations or when the fix does not fit a
repair rule (e.g. updating body content with `--body-from-stdin`).

## Common pitfalls

- **Don't filter by un-indexed fields.** `vault find` predicates match frontmatter scalar or list values only for field-equality flags; `--text` is for full-text substring search.
- **Honor schema versions.** Repair plans have `schema_version: 8` as of v0.32. Older plans are rejected by apply.
- **Don't auto-pick ambiguous link candidates.** `link-ambiguous` findings carry a `candidates` list, but the CLI does not automatically resolve them. An agent should surface the ambiguity to the human or apply a deterministic disambiguation rule documented in the vault's config.
- **Don't redirect to a file when `--out` exists.** `vault repair plan --out repair.json` is the file-first form; shell redirection works too but `--out` makes the intent explicit and avoids partial-write footguns.
- **User-specific vault doctrine lives in `.vault/config.yaml`.** Don't hardcode vault-specific rule names or field shapes in agent prompts; read them from the config.

## Skill installation

For per-harness install instructions (Claude Code, Codex, Open Code, OpenClaw, Hermes, PI), see [integrations/agent-skill/README.md](../integrations/agent-skill/README.md). The skill body itself is harness-independent and lives at [integrations/agent-skill/SKILL.md](../integrations/agent-skill/SKILL.md).

## See also

- [Commands](commands.md) — the full subcommand surface.
- [Validation and repair](validation.md) — finding codes and the apply contract.
- [Configuration](configuration.md) — config keys an agent might read.
