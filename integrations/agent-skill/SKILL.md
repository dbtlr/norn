---
name: vault-cli
description: Use when inspecting, validating, or auditing Markdown vaults with the `vault` CLI. Provides deterministic graph, link, frontmatter, and validation workflows.
version: 1.0.0
author: Drew Butler <hi@dbtlr.com>
license: MIT
---

# vault-cli skill

A deterministic Markdown vault CLI. Use it for graph, link, frontmatter, and validation work against a vault on disk. This skill is harness-independent — every coding agent that follows the standard `.agents/skills/` convention (or `.claude/skills/` for Claude Code) can use it.

## When to use vault

Use `vault` when you need to:

- Inspect a Markdown vault's document inventory, frontmatter, or link graph deterministically.
- Validate a vault against configured rules (`required_frontmatter`, `field_types`, `allowed_values`, `allowed_paths`, etc.).
- Audit unresolved or ambiguous links.
- Surface frontmatter drift for review.
- Produce an inspectable repair plan (`schema_version: 8`) and apply it explicitly.

Do not use `vault` when you need full-text or semantic search — its `find` command is exact literal substring + frontmatter + path glob.

## Vault root targeting

Before running any command, pick a vault root. Two ways:

1. **Explicit path.** `vault -C /path/to/vault validate --summary --format json` (long form: `--cwd /path/to/vault`).
2. **Process cwd.** If `-C` is not set, `vault` runs against the current directory and discovers `.vault/config.yaml` if it exists.

When in doubt, use `-C <path>`.

## Read-only commands (safe to run anytime)

None of these write to the vault:

- `vault find --all` (document inventory)
- `vault count` / `vault count --by FIELD`
- `vault get <doc>`
- `vault find`
- `vault validate` (with or without `--summary` or filters)
- `vault repair plan` (produces an artifact; does not modify the vault)

`vault set`, `vault move`, and `vault delete` are mutation commands; pass `--dry-run` to preview without writing. Only `vault repair apply`, `vault set`, `vault move`, and `vault delete` (without `--dry-run`) write to the vault. The repair plan argument is optional — omit it (or pass `-`) to read the plan from stdin.

## Validation summary first, raw findings second

`vault validate --summary --format json` returns grouped counts (by code, severity, rule, field, disallowed value, path prefix). Always run the summary first to size the work, then re-run without `--summary` to read individual findings.

```bash
vault -C /path/to/vault validate --summary --format json
vault -C /path/to/vault validate --code frontmatter-disallowed-value --field status --summary --format json
vault -C /path/to/vault validate --code frontmatter-disallowed-value --field status --format jsonl
```

The same filter set works for raw output and summaries.

## Stable JSON / JSONL contracts

Use `--format json` for one-shot agent dispatch (single JSON document). Use `--format jsonl` for streaming queues (one JSON object per line). Records output is for humans and may evolve between point releases — never parse it.

> **Note for `vault repair plan` and `vault repair apply`:** The repair format set is separate from validate. Use `--format json` (machine, full envelope — canonical for agent consumers; default when stdout is a pipe), `--format report` (human-readable summary, TTY default), or `--format paths` (one path per line, for `xargs`-style pipelines). `--format jsonl` is not supported for repair commands.
>
> `--out <PATH>` writes the JSON report/plan to a file unconditionally (always JSON). `--format` controls stdout independently — both can be set simultaneously without conflict. When `--out` is set without `--format`, stdout is silent.

Findings come back wrapped as `{"total": N, "findings": [...]}`; iterate `.findings` for individual entries.

Finding codes are stable. Renames are called out as breaking changes in the project's CHANGELOG.

## Filter-based triage

`vault validate` filters apply to both raw output and `--summary`:

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

## User-specific vault doctrine lives in .vault/config.yaml

Don't hardcode vault-specific rule names, field shapes, or status vocabularies into agent prompts. Read them from `<vault-root>/.vault/config.yaml`. The config declares:

- `files.ignore` — graph-level ignores.
- `validate.ignore` — validate-skip patterns (the files stay in the graph).
- `validate.required_frontmatter` — global presence requirement.
- `validate.rules` — scoped rules with selectors and constraints.
- `repair.rules` — deterministic frontmatter repairs.

If a vault has no config, defaults apply.

## Plan/apply boundary

Two write surfaces exist. Use the right one for the job:

- **`vault set` / `vault move` / `vault delete`** — operator-driven CRUD. One document, one command. Schema-aware, safe-by-default (dry-run preview, `--yes` to apply).
- **`vault repair apply`** — finding-driven batch repair. Consumes a plan artifact produced by `vault repair plan`. Apply checks document hashes; any precondition failure aborts the whole batch before any partial writes.

1. `vault -C /path/to/vault repair plan --out repair.json`
2. Inspect `repair.json`. Read `summary.planned_changes` count and `summary.skipped.by_reason` map for skip tallies.
3. `vault -C /path/to/vault repair apply repair.json --dry-run --format json` — confirms the plan applies cleanly.
4. `vault -C /path/to/vault repair apply repair.json --verify --format json` — writes and re-validates.

**Single-line pipeline form** (avoids `--out` round-trips):
```bash
vault -C /path/to/vault repair plan --format json | vault -C /path/to/vault repair apply [--dry-run]
```
`vault repair apply -` and `vault repair apply` (no positional argument) both read the plan from stdin.

Apply rejects:

- Plans for a different vault root than the current invocation.
- Stale document hashes (a file changed since the plan was created).
- Unsupported schema versions (currently `8`).
- Conflicting field changes.
- Expected-old-value mismatches.

Re-plan rather than retrying. There is no `--force` flag.

### Repair action shapes

vault-cli supports six repair actions:

- `set_frontmatter` — replace an existing frontmatter field's value.
- `remove_frontmatter` — remove a frontmatter field.
- `add_frontmatter` — insert a missing frontmatter field.
- `move_document` — relocate (or rename) a file to a new path, with automatic backlink rewriting.
- `rewrite_link` — rewrite a broken wikilink in the source document to a new target. Preserves display text (`[[X|label]]`), anchor (`[[X#section]]`), and block-ref (`[[X^block-id]]`) suffixes. All matching occurrences in the source are rewritten.
- `replace_body` — wholesale replacement of the document body. **Emitted only by `vault set --body-from-stdin`; this is not a config-rule-triggerable action.** Operators cannot write `replace_body` in a `repair.rules` config entry.

Agents should not invent destination paths or values for these actions. The repair rule (or closest-match algorithm for `rewrite_link`) supplies the target value at plan time — no agent judgment required.

When a move action is in the plan, expect `repair apply` to write to multiple files: the moved file itself and every backlinking file that contains a rewritable link. The apply output's `moved_files` and `rewritten_links` enumerate everything that was touched.

### Closest-match link rewrites

For `link-target-missing` findings, `vault repair plan` proposes closest-match `rewrite_link` changes automatically:

- **High-confidence** proposals: slug-normalized identity match (case, whitespace, hyphen/underscore variants). Safe to apply without review.
- **Medium-confidence** proposals: small residual edit distance (Levenshtein ratio ≥ 0.7). Review recommended before applying.
- **Ties** (multiple equally-close candidates): skipped with `reason_code: "ambiguous-target"` (Rust-side: `SkipReason::AmbiguousTarget`); the candidate list is populated for human review.

Use `--confidence high` to filter the plan to only high-confidence proposals (drops medium proposals and their footnotes). Default emits all confidence bands.

Use `--skip-reason <PATTERN>` to filter the `skipped_findings` list by stable reason code. Useful for triage: `--skip-reason 'link-*'` shows only link-related skips; `--skip-reason ambiguous-target` isolates multi-candidate ties. Glob patterns accepted. Repeatable. Does not affect planned changes.

Stable reason codes: `missing-default`, `link-decision-needed`, `no-rule-matched`, `alias-shadowed`, `graph-diagnostic`, `ambiguous-target`, `missing-hash`, `precondition-failed`.

### Plan footnotes

Repair plans (schema v8) carry a `footnotes` array alongside `changes`. Footnotes are read-only commentary — `repair apply` ignores them entirely; they exist for LLM/operator consumers to reason about proposal quality. Each footnote for a closest-match rewrite carries:

- `change_id` — references the corresponding change.
- `confidence` — `high` or `medium`.
- `original_target`, `normalized_target`, `candidate_stem` — the raw and normalized forms.
- `normalized_distance` — Levenshtein ratio (1.0 = exact slug match).
- `slug_normalized_identity` — `true` when the match is case/whitespace/hyphen-only.

## vault set — targeted frontmatter and body mutation

`vault set` is the operator-facing write surface for single-document updates. Use it when
`vault validate` surfaces drift on a known document and you want to fix it without a full
repair-plan cycle, or when no repair rule covers the needed change (e.g. body replacement).

```bash
# Update a frontmatter field
vault set notes/task.md --field status=active --dry-run
vault set notes/task.md --field status=active --yes

# Append to an array-typed field
vault set notes/task.md --push tags=work --yes

# Remove a field
vault set notes/task.md --remove old_key --yes

# Wholesale body replacement
echo "new body content" | vault set notes/task.md --body-from-stdin --yes

# JSON output for agent consumers
vault set notes/task.md --field status=active --yes --format json
```

Safe-by-default: in a TTY, `vault set` shows a preview and prompts for confirmation.
Without `--yes` in a non-TTY context, it prints a dry-run summary and exits.

Schema enforcement: when `field_types` is configured, type validation runs before apply.
Use `--force` to bypass schema enforcement (not recommended unless you know the type mismatch
is intentional).

Exit codes: 0 success or dry-run, 1 operator-cancelled, 2 pre-flight refusal.

## Typical agent loop

```bash
# 1. Detect
vault -C /path/to/vault validate --summary --format json

# 2. Triage (size the queue)
vault -C /path/to/vault validate \
  --code frontmatter-disallowed-value \
  --field status \
  --summary --format json

# 3. Plan
vault -C /path/to/vault repair plan \
  --code frontmatter-disallowed-value \
  --field status \
  --out repair.json

# 4. Review
# (read repair.json; surface skipped_findings to the human if non-empty)

# 5. Dry-run (--format json = full RepairApplyReport envelope)
vault -C /path/to/vault repair apply repair.json --dry-run --format json

# 6. Apply with verification
vault -C /path/to/vault repair apply repair.json --verify --format json

# Alternative: single-line pipeline (skips the repair.json file entirely)
vault -C /path/to/vault repair plan --format json | vault -C /path/to/vault repair apply --verify
```

## Cache

`vault` maintains a SQLite cache of the graph (documents, links, headings, frontmatter, body text). Query commands automatically refresh the cache before reading; agents don't need to run `vault cache index` explicitly in the common case.

If you observe stale results, try `vault cache rebuild` to force a clean state.

The cache is disposable — missing or corrupted caches rebuild silently. Don't program around cache-error retries; surface them as bugs to fix.

## Common pitfalls

- **Don't filter by un-indexed fields.** `vault find --eq FIELD:VALUE` matches frontmatter scalar or list values only. For body text matching, use `vault find --text "..."`.
- **Honor schema versions.** Repair plans declare `schema_version`. Apply rejects mismatched versions; re-plan instead of editing the artifact.
- **Don't auto-pick ambiguous link candidates.** `link-ambiguous` findings carry a `candidates` list, but the CLI does not resolve them. Surface the ambiguity to the human or apply a deterministic disambiguation rule documented in the vault's config.
- **Use `--out` for plan artifacts.** `vault repair plan --out repair.json` writes the plan directly. Shell redirection (`> repair.json`) works but is more prone to partial-write footguns.
- **Don't parse records output.** Records are for humans. For `vault validate`, pass `--format json` or `--format jsonl` from an agent context. For `vault repair plan` and `vault repair apply`, pass `--format json` (full envelope) or `--format paths` (changed-files list).
- **Run `--summary` first.** It's cheaper than a full finding stream and tells you whether a more expensive query is worth running.

## Shell completions

If you're driving vault on a user's machine and they want vault commands tab-completable, run:

```bash
vault completions install
```

This auto-detects the user's shell from `$SHELL` and wires completions into their shell config idempotently. Pass `--print` to preview without writing.

## Reference

- CLI command surface: `vault --help`, `vault <subcommand> --help`.
- Repository: https://github.com/dbtlr/vault-cli
- Documentation: https://github.com/dbtlr/vault-cli/tree/main/docs
- Agent workflow guide: https://github.com/dbtlr/vault-cli/blob/main/docs/agent-workflows.md
