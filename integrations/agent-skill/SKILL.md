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
- Produce an inspectable repair plan (`schema_version: 4`) and apply it explicitly.

Do not use `vault` when you need full-text or semantic search — its `find` command is exact literal substring + frontmatter + path glob.

## Vault root targeting

Before running any command, pick a vault root. Two ways:

1. **Explicit path.** `vault -C /path/to/vault validate --summary --format json` (long form: `--cwd /path/to/vault`).
2. **Process cwd.** If `-C` is not set, `vault` runs against the current directory and discovers `.vault/config.yaml` if it exists.

When in doubt, use `-C <path>`.

## Read-only commands (safe to run anytime)

None of these write to the vault:

- `vault docs list / summary / inspect`
- `vault files`
- `vault links list / unresolved / backlinks`
- `vault find`
- `vault validate` (with or without `--summary` or filters)
- `vault repair plan` (produces an artifact; does not modify the vault)
- `vault repair links` (planning report only)

Only `vault repair apply` writes to the vault. It requires an explicit plan argument.

## Validation summary first, raw findings second

`vault validate --summary --format json` returns grouped counts (by code, severity, rule, field, disallowed value, path prefix). Always run the summary first to size the work, then re-run without `--summary` to read individual findings.

```bash
vault -C /path/to/vault validate --summary --format json
vault -C /path/to/vault validate --code frontmatter-disallowed-value --field status --summary --format json
vault -C /path/to/vault validate --code frontmatter-disallowed-value --field status --format jsonl
```

The same filter set works for raw output and summaries.

## Stable JSON / JSONL contracts

Use `--format json` for one-shot agent dispatch (single JSON document). Use `--format jsonl` for streaming queues (one JSON object per line). Table output is for humans and may evolve between point releases — never parse it.

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

Comma-separated values within one filter are ORed (`--code link-unresolved,link-ambiguous`); different filters are ANDed.

## User-specific vault doctrine lives in .vault/config.yaml

Don't hardcode vault-specific rule names, field shapes, or status vocabularies into agent prompts. Read them from `<vault-root>/.vault/config.yaml`. The config declares:

- `files.ignore` — graph-level ignores.
- `validate.ignore` — validate-skip patterns (the files stay in the graph).
- `validate.required_frontmatter` — global presence requirement.
- `validate.rules` — scoped rules with selectors and constraints.
- `repair.rules` — deterministic frontmatter repairs.

If a vault has no config, defaults apply.

## Plan/apply boundary

Mutation is always two steps. Never write to the vault outside `vault repair apply`.

1. `vault -C /path/to/vault repair plan --out repair.json`
2. Inspect `repair.json`. Read `summary.planned_changes` and `summary.skipped.*` counts.
3. `vault -C /path/to/vault repair apply repair.json --dry-run --format json` — confirms the plan applies cleanly.
4. `vault -C /path/to/vault repair apply repair.json --verify --format json` — writes and re-validates.

Apply rejects:

- Plans for a different vault root than the current invocation.
- Stale document hashes (a file changed since the plan was created).
- Unsupported schema versions (currently `4`).
- Conflicting field changes.
- Expected-old-value mismatches.

Re-plan rather than retrying. There is no `--force` flag.

### Repair action shapes

vault-cli supports four repair actions in `.vault/config.yaml`:

- `set_frontmatter` — replace an existing frontmatter field's value.
- `remove_frontmatter` — remove a frontmatter field.
- `add_frontmatter` — insert a missing frontmatter field.
- `move_document` — relocate (or rename) a file to a new path, with automatic backlink rewriting.

Agents should not invent destination paths or values for these actions. The repair rule supplies the literal value (`set_frontmatter`, `add_frontmatter`, `remove_frontmatter`) or the destination spec (`move_document`'s `to_directory` or `to_path`). Substitution placeholders (`{stem}`, `{filename}`, `{frontmatter.<field>}`) are resolved at plan time using the file's actual state — no agent judgment required.

When a move action is in the plan, expect `repair apply` to write to multiple files: the moved file itself and every backlinking file that contains a rewritable link. The apply output's `moved_files` and `rewritten_links` enumerate everything that was touched.

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

# 5. Dry-run
vault -C /path/to/vault repair apply repair.json --dry-run --format json

# 6. Apply with verification
vault -C /path/to/vault repair apply repair.json --verify --format json
```

## Cache

`vault` maintains a SQLite cache of the graph (documents, links, headings, frontmatter, body text). Query commands automatically refresh the cache before reading; agents don't need to run `vault cache index` explicitly in the common case.

If you observe stale results, try `vault cache rebuild` to force a clean state.

The cache is disposable — missing or corrupted caches rebuild silently. Don't program around cache-error retries; surface them as bugs to fix.

## Common pitfalls

- **Don't filter by un-indexed fields.** `vault docs list --filter` matches frontmatter scalar or list values only. For body text matching, use `vault find --text "..."`.
- **Honor schema versions.** Repair plans declare `schema_version`. Apply rejects mismatched versions; re-plan instead of editing the artifact.
- **Don't auto-pick ambiguous link candidates.** `link-ambiguous` findings carry a `candidates` list, but the CLI does not resolve them. Surface the ambiguity to the human or apply a deterministic disambiguation rule documented in the vault's config.
- **Use `--out` for plan artifacts.** `vault repair plan --out repair.json` writes the plan directly. Shell redirection (`> repair.json`) works but is more prone to partial-write footguns.
- **Don't parse table output.** Tables are for humans. Always pass `--format json` or `--format jsonl` from an agent context.
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
