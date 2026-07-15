---
title: Agent workflows
description: Stable JSON and JSONL contracts, agent loop patterns, and common harness gotchas for driving norn from a coding agent.
---

# Agent workflows

`norn` is designed to be a first-class tool for coding agents. This page documents the contracts an agent can rely on, recommended loop patterns, and common pitfalls.

## Stable contracts

| Contract | Surface | Stability |
|---|---|---|
| JSON output | `--format json` on every command | Stable across point releases; breaking changes called out in CHANGELOG. |
| JSONL output | `--format jsonl` on every command | Same. |
| Paths output | `--format paths` on commands that emit per-row paths | Stable; one unique vault-relative path per row. |
| Migration plan schema | `repair --plan` JSON artifact (`MigrationPlan`) | Schema-versioned (`schema_version` field). Apply rejects mismatched versions. |
| Apply report schema | `apply` JSON output (`ApplyReport`) | Stable across the matching plan schema version. |
| Finding codes | `norn validate` output `code` field | Stable; renames are breaking changes called out in CHANGELOG. |

Default human-readable rendering (`records` on most commands, `report` for `repair --plan`) is for humans and may evolve between point releases. Agents should always pass an explicit `--format json` or `--format jsonl`.

## Vault targeting

An agent should detect the vault root before running any command. The two ways:

1. **`-C <path>` (alias `--cwd`).** One-shot invocation against an arbitrary directory.
   ```bash
   norn -C /path/to/vault validate --summary --format json
   ```
2. **Process cwd.** When `-C` is not set, `norn` runs against the current directory. Discovery of `.norn/config.yaml` is implicit.

`--cwd PATH` is the only vault-targeting mechanism. An agent operating on multiple vaults should pass `-C` per command.

## Recommended agent loop

For a typical drift-healing task:

1. **Detect.** `norn validate --summary --format json` — get a finding shape before reading individuals.
2. **Triage.** Filter by `--code`, `--field`, `--rule`, `--path` to scope the queue. Re-run `--summary` to confirm the filter's size.
3. **Plan.** `norn repair --plan --out plan.json` (with the same filters). Read the plan's `changes` and `skipped_findings`.
4. **Review.** Confirm `changes` are intended; surface `skipped_findings` to the human or follow `next_actions`.
5. **Dry-run.** `norn apply plan.json --dry-run --format json` — confirms the plan is applyable without writing. (Or pipe directly: `norn repair --plan --format json | norn apply - --dry-run --format json`.)
6. **Apply.** `norn apply plan.json --format json --yes` — writes. (`--format json` is output-shape-only; `--yes` is what gives consent to write — omit it and the same invocation is an implicit dry-run.) Every frontmatter write is re-parsed and checked against the intended value before apply reports success (the post-image verification gate); there is no separate `--verify` flag.
7. **Verify.** Inspect the apply report's `operations` and `warnings`, then run `norn validate --summary --format json` again as the post-hoc check that the vault is now clean.

For a read-only inspection task (no mutation):

1. `norn find --all --format json` or `norn count --by <field> --format json`.
2. `norn validate --summary --format json` to spot drift.
3. `norn get <target> --format json` for one-document detail.

## Read-only commands

These commands never write to the vault. An agent can run them with confidence:

- `norn find`
- `norn count`
- `norn describe` (with or without `--data`/`--stats`/`--by`)
- `norn get`
- `norn validate` (with or without `--summary`, with or without filters)
- `norn repair --plan` (produces a `MigrationPlan` artifact; does not modify the vault)
- `norn audit`

`norn new`, `norn set`, `norn move`, `norn delete`, and `norn apply` are mutation commands; pass `--dry-run` to preview without writing. Only `norn apply`, `norn new`, `norn set`, `norn move`, and `norn delete` (without `--dry-run`) write to the vault. The migration plan is provided to `norn apply` via a positional file path, via `-`, or via stdin (the pipeline form `norn repair --plan --format json | norn apply -` composes plan generation and apply in one shot).

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
{"code":"value-not-allowed","severity":"warning","path":"tasks/triage.md","rule":"task-status","field":"status","actual_value":"someday","allowed_values":["backlog","in_progress","completed","wont_do"]}
```

### Migration plan (JSON)

```json
{
  "schema_version": 1,
  "vault_root": "/abs/path/to/vault",
  "source_filters": { "code": "value-not-allowed", "field": "status" },
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
  "schema_version": 2,
  "trace_id": "…",
  "plan_hash": "…",
  "vault_root": "/abs/path/to/vault",
  "dry_run": false,
  "applied": 3,
  "skipped": 0,
  "failed": 0,
  "remaining": 0,
  "operations": [ { "op_id": "…", "kind": "set_frontmatter", "status": "applied", "summary": "…" } ],
  "warnings": []
}
```

Every frontmatter write in `operations` passes a post-image verification gate before the apply reports it as `applied` — there is no separate opt-in verify step to request. Re-run `norn validate --summary --format json` afterward as the post-hoc check for drift the plan didn't cover.

## Filter-based triage

The filter dimensions on `norn validate` (`--code`, `--severity`, `--field`, `--rule`, `--path`, `--target`, `--reason`) are designed for agent-driven triage. Comma-separated values within one filter are ORed; different filters are ANDed.

Use `--summary` first to size a queue, then re-run without `--summary` to read the queue itself:

```bash
norn validate --code field-type-invalid --field modified --summary --format json
norn validate --code field-type-invalid --field modified --format jsonl
```

## Plan/apply boundary

Two rules an agent must follow:

1. **Use the appropriate write surface.** For creating a new document from a schema scaffold, use `norn new`. For operator-driven one-doc mutations on existing docs, `norn set` (frontmatter + body), `norn move`, and `norn delete` are the CRUD surface. For finding-driven batch repairs, `norn apply` is the only path — it consumes a `MigrationPlan` artifact and applies deterministic changes with precondition checks. Never edit vault files directly; the graph state would diverge from the cache.
2. **Always pass the plan that matches the current vault state.** Apply checks document hashes; if a file changed since the plan was created, the change is rejected for that file. Re-plan rather than re-apply with `--force` (there is no `--force` for apply).

`--dry-run` confirms the plan is applyable without writing. Apply itself verifies every frontmatter write against its intended value before reporting success (no opt-in flag needed); run `norn validate --summary` afterward as the post-hoc check across the whole vault.

## Using norn set for targeted frontmatter updates

When `norn validate` surfaces drift on a single document, `norn set` is the natural follow-up
for a targeted fix without a full plan/apply cycle:

```bash
# 1. Validate — surface a disallowed-value finding on one doc
norn validate --code value-not-allowed --field status --format jsonl

# 2. Fix — update that document's status field directly
norn set notes/task.md --field status=backlog --dry-run
norn set notes/task.md --field status=backlog --yes

# 3. Re-validate — confirm the finding is gone
norn validate --code value-not-allowed --field status --format jsonl
```

For batch fixes across many documents, prefer the `repair --plan` → `apply` loop.
`norn set` is best for targeted one-doc mutations or when the fix does not fit a
repair rule (e.g. updating body content with `--body-from-stdin`).

## Common pitfalls

- **Don't filter by un-indexed fields.** `norn find` predicates match frontmatter scalar or list values only for field-equality flags; `--text` is for full-text substring search.
- **Honor schema versions.** Migration plans (`MigrationPlan`) have `schema_version: 2`. Older v1 `MigrationPlan` and schema-9 `RepairPlan` artifacts are rejected; re-plan with `norn repair --plan`.
- **Don't auto-pick ambiguous link candidates.** `link-ambiguous` findings carry a `candidates` list, but the CLI does not automatically resolve them. An agent should surface the ambiguity to the human or apply a deterministic disambiguation rule documented in the vault's config.
- **Don't redirect to a file when `--out` exists.** `norn repair --plan --out plan.json` is the file-first form; shell redirection works too but `--out` makes the intent explicit and avoids partial-write footguns.
- **User-specific vault doctrine lives in `.norn/config.yaml`.** Don't hardcode vault-specific rule names or field shapes in agent prompts; read them from the config.

## MCP server

Beyond the CLI, norn can expose the vault to an MCP client as a set of tools via `norn mcp` — the same deterministic primitives (find, count, get, validate, repair-plan, plus the full mutation surface) reachable over stdio by an agent whose vault may be remote or off-filesystem. The mutation tools are dry-run by default and apply (under the per-vault mutation lock, audited to the event stream) only on `confirm: true` — the MCP analog of the CLI's `--dry-run` / apply split.

See [MCP server](mcp-server.md) for the 14-tool catalog, the document-placement workflow (`vault.describe` → construct path → `vault.new`), and the warm-cache and call-ordering notes.

## Skill installation

For per-harness install instructions (Claude Code, Codex, Open Code, OpenClaw, Hermes, PI), see [integrations/agent-skill/README.md](../integrations/agent-skill/README.md). The skill body itself is harness-independent and lives at [integrations/agent-skill/SKILL.md](../integrations/agent-skill/SKILL.md).

## See also

- [Commands](commands.md) — the full subcommand surface.
- [MCP server](mcp-server.md) — driving the same primitives as MCP tools.
- [Validation and repair](validation.md) — finding codes and the apply contract.
- [Configuration](configuration.md) — config keys an agent might read.
