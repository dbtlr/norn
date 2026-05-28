---
name: norn
description: Use when inspecting, validating, or auditing Markdown vaults with the `norn` CLI. Provides deterministic graph, link, frontmatter, and validation workflows.
version: 1.0.0
author: Drew Butler <hi@dbtlr.com>
license: MIT
---

# norn skill

A deterministic Markdown vault CLI. Use it for graph, link, frontmatter, and validation work against a vault on disk. This skill is harness-independent — every coding agent that follows the standard `.agents/skills/` convention (or `.claude/skills/` for Claude Code) can use it.

## When to use norn

Use `norn` when you need to:

- Inspect a Markdown vault's document inventory, frontmatter, or link graph deterministically.
- Validate a vault against configured rules (`required_frontmatter`, `field_types`, `allowed_values`, `allowed_paths`, etc.).
- Audit unresolved or ambiguous links.
- Surface frontmatter drift for review.
- Produce an inspectable migration plan (`MigrationPlan`, `schema_version: 1`) and apply it explicitly.

Do not use `norn` when you need full-text or semantic search — its `find` command is exact literal substring + frontmatter + path glob.

## Vault root targeting

Before running any command, pick a vault root. Two ways:

1. **Explicit path.** `norn -C /path/to/vault validate --summary --format json` (long form: `--cwd /path/to/vault`).
2. **Process cwd.** If `-C` is not set, `norn` runs against the current directory and discovers `.norn/config.yaml` if it exists.

When in doubt, use `-C <path>`.

## Read-only commands (safe to run anytime)

None of these write to the vault:

- `norn find --all` (document inventory)
- `norn count` / `norn count --by FIELD`
- `norn get <doc>`
- `norn find`
- `norn validate` (with or without `--summary` or filters)
- `norn repair --plan` (produces a `MigrationPlan` artifact; does not modify the vault)

`norn new`, `norn set`, `norn move`, `norn delete`, and `norn migrate` are mutation commands; pass `--dry-run` to preview without writing. Only `norn migrate`, `norn new`, `norn set`, `norn move`, and `norn delete` (without `--dry-run`) write to the vault. The migration plan argument is optional — omit it (or pass `-`) to read the plan from stdin.

## Validation summary first, raw findings second

`norn validate --summary --format json` returns grouped counts (by code, severity, rule, field, disallowed value, path prefix). Always run the summary first to size the work, then re-run without `--summary` to read individual findings.

```bash
norn -C /path/to/vault validate --summary --format json
norn -C /path/to/vault validate --code frontmatter-disallowed-value --field status --summary --format json
norn -C /path/to/vault validate --code frontmatter-disallowed-value --field status --format jsonl
```

The same filter set works for raw output and summaries.

## Stable JSON / JSONL contracts

Use `--format json` for one-shot agent dispatch (single JSON document). Use `--format jsonl` for streaming queues (one JSON object per line). Records output is for humans and may evolve between point releases — never parse it.

> **Note for `norn repair --plan` and `norn migrate`:** The plan/migrate format set is separate from validate. For `norn repair --plan`, use `--format json` (machine, full `MigrationPlan` envelope — canonical for agent consumers; default when stdout is a pipe), `--format report` (human-readable summary, TTY default), or `--format paths` (one path per line, for `xargs`-style pipelines). `--format jsonl` is not supported. For `norn migrate`, use `--format json` (full `ApplyReport` envelope), `--format records` (TTY default), or `--format paths` (changed-files list).
>
> `--out <PATH>` writes the JSON report/plan to a file unconditionally (always JSON). `--format` controls stdout independently — both can be set simultaneously without conflict. When `--out` is set without `--format`, stdout is silent.

Findings come back wrapped as `{"total": N, "findings": [...]}`; iterate `.findings` for individual entries.

Finding codes are stable. Renames are called out as breaking changes in the project's CHANGELOG.

## Filter-based triage

`norn validate` filters apply to both raw output and `--summary`:

| Filter | Matches |
|---|---|
| `--code` | Finding code. |
| `--severity` | `warning` or `error`. |
| `--field` | Frontmatter field name. |
| `--rule` | Rule name. |
| `--path` | Vault-relative path glob. |
| `--target` | Raw parsed link target string (exact match). |
| `--reason` | Unresolved-link reason. |

Comma-separated values within one filter are ORed (`--code link-target-missing,link-ambiguous`), and glob patterns work (`--code 'link-*'`); different filters are ANDed.

## User-specific vault doctrine lives in .norn/config.yaml

Don't hardcode vault-specific rule names, field shapes, or status vocabularies into agent prompts. Read them from `<vault-root>/.norn/config.yaml`. The config declares:

- `files.ignore` — graph-level ignores.
- `validate.ignore` — validate-skip patterns (the files stay in the graph).
- `validate.required_frontmatter` — global presence requirement.
- `validate.rules` — scoped rules with selectors and constraints.
- `repair.rules` — deterministic frontmatter repairs.

If a vault has no config, defaults apply.

## Plan/apply boundary

Two write surfaces exist. Use the right one for the job:

- **`norn new` / `norn set` / `norn move` / `norn delete`** — operator-driven CRUD. One document, one command. Schema-aware, safe-by-default (dry-run preview, `--yes` to apply).
- **`norn migrate`** — finding-driven batch apply. Consumes a `MigrationPlan` artifact produced by `norn repair --plan`. Apply checks document hashes; any precondition failure aborts the whole batch before any partial writes.

1. `norn -C /path/to/vault repair --plan --out plan.json`
2. Inspect `plan.json`. Read `summary.planned_changes` count and `summary.skipped.by_reason` map for skip tallies.
3. `norn -C /path/to/vault migrate plan.json --dry-run --format json` — confirms the plan applies cleanly.
4. `norn -C /path/to/vault migrate plan.json --verify --format json` — writes and re-validates.

**Single-line pipeline form** (avoids `--out` round-trips):
```bash
norn -C /path/to/vault repair --plan --format json | norn -C /path/to/vault migrate - [--dry-run]
```
`norn migrate -` and `norn migrate` (no positional argument) both read the plan from stdin.

Apply rejects:

- Plans for a different norn root than the current invocation.
- Stale document hashes (a file changed since the plan was created).
- Unsupported schema versions (currently `1`).
- Conflicting field changes.
- Expected-old-value mismatches.

Re-plan rather than retrying. There is no `--force` flag.

### Repair action shapes

norn supports seven repair actions:

- `set_frontmatter` — replace an existing frontmatter field's value.
- `remove_frontmatter` — remove a frontmatter field.
- `add_frontmatter` — insert a missing frontmatter field.
- `move_document` — relocate (or rename) a file to a new path, with automatic backlink rewriting.
- `rewrite_link` — rewrite a broken wikilink in the source document to a new target. Preserves display text (`[[X|label]]`), anchor (`[[X#section]]`), and block-ref (`[[X^block-id]]`) suffixes. All matching occurrences in the source are rewritten.
- `replace_body` — wholesale replacement of the document body. **Emitted only by `norn set --body-from-stdin`; this is not a config-rule-triggerable action.** Operators cannot write `replace_body` in a `repair.rules` config entry.
- `create_document` — create a brand-new document with synthesized frontmatter and body. Emitted exclusively by `norn new`; not config-rule-triggerable. (Sibling to `replace_body`, which is `norn set --body-from-stdin`'s plan op.)

Agents should not invent destination paths or values for these actions. The repair rule (or closest-match algorithm for `rewrite_link`) supplies the target value at plan time — no agent judgment required.

When a move action is in the plan, expect `norn migrate` to write to multiple files: the moved file itself and every backlinking file that contains a rewritable link. The apply output's `moved_files` and `rewritten_links` enumerate everything that was touched.

### Closest-match link rewrites

For `link-target-missing` findings, `norn repair --plan` proposes closest-match `rewrite_link` changes automatically:

- **High-confidence** proposals: slug-normalized identity match (case, whitespace, hyphen/underscore variants). Safe to apply without review.
- **Medium-confidence** proposals: small residual edit distance (Levenshtein ratio ≥ 0.7). Review recommended before applying.
- **Ties** (multiple equally-close candidates): skipped with `reason_code: "ambiguous-target"` (Rust-side: `SkipReason::AmbiguousTarget`); the candidate list is populated for human review.

Use `--confidence high` to filter the plan to only high-confidence proposals (drops medium proposals and their footnotes). Default emits all confidence bands.

Use `--skip-reason <PATTERN>` to filter the `skipped_findings` list by stable reason code. Useful for triage: `--skip-reason 'link-*'` shows only link-related skips; `--skip-reason ambiguous-target` isolates multi-candidate ties. Glob patterns accepted. Repeatable. Does not affect planned changes.

Stable reason codes: `missing-default`, `link-decision-needed`, `no-rule-matched`, `alias-shadowed`, `graph-diagnostic`, `ambiguous-target`, `missing-hash`, `precondition-failed`.

### Plan footnotes

Migration plans (`MigrationPlan`, schema v1) carry a `footnotes` array alongside `changes`. Footnotes are read-only commentary — `norn migrate` ignores them entirely; they exist for LLM/operator consumers to reason about proposal quality. Each footnote for a closest-match rewrite carries:

- `change_id` — references the corresponding change.
- `confidence` — `high` or `medium`.
- `original_target`, `normalized_target`, `candidate_stem` — the raw and normalized forms.
- `normalized_distance` — Levenshtein ratio (1.0 = exact slug match).
- `slug_normalized_identity` — `true` when the match is case/whitespace/hyphen-only.

## norn set — targeted frontmatter and body mutation

`norn set` is the operator-facing write surface for single-document updates. Use it when
`norn validate` surfaces drift on a known document and you want to fix it without a full
repair-plan cycle, or when no repair rule covers the needed change (e.g. body replacement).

```bash
# Update a frontmatter field
norn set notes/task.md --field status=active --dry-run
norn set notes/task.md --field status=active --yes

# Append to an array-typed field
norn set notes/task.md --push tags=work --yes

# Remove a field
norn set notes/task.md --remove old_key --yes

# Wholesale body replacement
echo "new body content" | norn set notes/task.md --body-from-stdin --yes

# JSON output for agent consumers
norn set notes/task.md --field status=active --yes --format json
```

Safe-by-default: in a TTY, `norn set` shows a preview and prompts for confirmation.
Without `--yes` in a non-TTY context, it prints a dry-run summary and exits.

Schema enforcement: when `field_types` is configured, type validation runs before apply.
Use `--force` to bypass schema enforcement (not recommended unless you know the type mismatch
is intentional).

Exit codes: 0 success or dry-run, 1 operator-cancelled, 2 pre-flight refusal.

## Typical agent loop

```bash
# 1. Detect
norn -C /path/to/vault validate --summary --format json

# 2. Triage (size the queue)
norn -C /path/to/vault validate \
  --code frontmatter-disallowed-value \
  --field status \
  --summary --format json

# 3. Plan
norn -C /path/to/vault repair --plan \
  --code frontmatter-disallowed-value \
  --field status \
  --out plan.json

# 4. Review
# (read plan.json; surface skipped_findings to the human if non-empty)

# 5. Dry-run (--format json = full ApplyReport envelope)
norn -C /path/to/vault migrate plan.json --dry-run --format json

# 6. Apply with verification
norn -C /path/to/vault migrate plan.json --verify --format json

# Alternative: single-line pipeline (skips the plan.json file entirely)
norn -C /path/to/vault repair --plan --format json | norn -C /path/to/vault migrate - --verify
```

## Cache

`norn` maintains a SQLite cache of the graph (documents, links, headings, frontmatter, body text). Query commands automatically refresh the cache before reading; agents don't need to run `norn cache index` explicitly in the common case.

If you observe stale results, try `norn cache rebuild` to force a clean state.

The cache is disposable — missing or corrupted caches rebuild silently. Don't program around cache-error retries; surface them as bugs to fix.

## Common pitfalls

- **Don't filter by un-indexed fields.** `norn find --eq FIELD:VALUE` matches frontmatter scalar or list values only. For body text matching, use `norn find --text "..."`.
- **Honor schema versions.** Migration plans (`MigrationPlan`) declare `schema_version: 1`. `norn migrate` rejects mismatched versions; re-plan with `norn repair --plan` instead of editing the artifact.
- **Don't auto-pick ambiguous link candidates.** `link-ambiguous` findings carry a `candidates` list, but the CLI does not resolve them. Surface the ambiguity to the human or apply a deterministic disambiguation rule documented in the vault's config.
- **Use `--out` for plan artifacts.** `norn repair --plan --out plan.json` writes the plan directly. Shell redirection (`> plan.json`) works but is more prone to partial-write footguns.
- **Don't parse records output.** Records are for humans. For `norn validate`, pass `--format json` or `--format jsonl` from an agent context. For `norn repair --plan`, pass `--format json` (full `MigrationPlan` envelope) or `--format paths` (affected-paths list). For `norn migrate`, pass `--format json` (full `ApplyReport` envelope) or `--format paths` (changed-files list).
- **Run `--summary` first.** It's cheaper than a full finding stream and tells you whether a more expensive query is worth running.

## Shell completions

If you're driving norn on a user's machine and they want norn commands tab-completable, run:

```bash
norn completions install
```

This auto-detects the user's shell from `$SHELL` and wires completions into their shell config idempotently. Pass `--print` to preview without writing.

## Reference

- CLI command surface: `norn --help`, `norn <subcommand> --help`.
- Repository: https://github.com/dbtlr/norn
- Documentation: https://github.com/dbtlr/norn/tree/main/docs
- Agent workflow guide: https://github.com/dbtlr/norn/blob/main/docs/agent-workflows.md
