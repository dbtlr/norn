---
name: norn
description: Use when inspecting, querying, validating, or mutating Markdown vaults with the `norn` CLI. Provides deterministic graph, link, frontmatter, query, and validation/repair workflows.
version: 1.3.0
author: Drew Butler <hi@dbtlr.com>
license: MIT
---

# norn skill

A deterministic Markdown vault CLI. Use it to query, validate, and mutate a vault on disk — frontmatter, links, headings, and the document graph — without grep/jq/sed pipelines. This skill is harness-independent: every coding agent that follows the standard `.agents/skills/` convention (or `.claude/skills/` for Claude Code) can use it.

## When to use norn

Use `norn` when you need to:

- Query a vault's documents by frontmatter, body text, path, or link relationship — and project exactly the fields you want.
- Inspect one document's frontmatter, headings, and links.
- Create, update, move, or delete documents with schema-aware, safe-by-default mutations.
- Validate a vault against configured rules (`required_frontmatter`, `field_types`, `allowed_values`, path scoping) and audit unresolved or ambiguous links.
- Produce an inspectable `MigrationPlan` and apply it explicitly.

Do not use `norn` for full-text relevance or semantic search — `find --text` is exact, case-insensitive substring matching, not ranked retrieval.

## Vault root targeting

Pick a vault root before running anything. Three ways, in precedence order:

1. **Explicit path.** `norn -C /path/to/vault validate --summary --format json` (long form `--cwd`).
2. **`NORN_ROOT` env var.** Export `NORN_ROOT=/path/to/vault` to make that the default root for every invocation, so `norn` runs against it from any directory without `-C`.
3. **Process cwd.** With neither set, `norn` runs against the current directory and discovers `.norn/config.yaml` if present.

When in doubt, pass `-C <path>`.

## Query and read — the everyday surface

`find` selects a *set* of documents by predicate; `get` selects *named* documents by identity. They share one output contract: the same `--col` vocabulary, the same formats, the same sort/paging. Learn it once.

### find

```bash
norn find --eq type:note --limit 5            # frontmatter equality; find defaults to 10
norn find --text "reorg" --format paths       # case-insensitive body substring
norn find --has aliases --col title,aliases   # narrow the fields shown
norn find --in type:note,log --sort modified --desc
norn find --links-to notes/my-note.md --format paths
norn find --unresolved-links --format paths   # documents with broken links
norn find --all --all-cols --format jsonl     # whole-vault structured dump
```

Predicates (all ANDed; comma-separated values inside `--in`/`--not-in` are ORed): `--text`, `--eq`, `--not-eq`, `--in`, `--not-in`, `--has`, `--missing`, `--before`, `--after`, `--on` (accepts `today`), `--path` (glob), `--links-to`, `--unresolved-links`. A bare `norn find` with no predicate prints its help — pass `--all` to dump the whole vault on purpose.

### get

```bash
norn get notes/my-note.md                     # frontmatter + headings + links
norn get "My Note"                            # resolve by stem (case-insensitive)
norn get a.md b.md --col title,status         # several docs, narrowed
norn get notes/my-note.md --col .incoming_links
norn get notes/my-note.md --all-cols --format json
norn get notes/my-note.md --format markdown   # rebuild the doc as Markdown (get-only)
```

A target is a path, a unique stem, or a wikilink-shaped string. `get` returns every named target (no default limit), unlike `find`.

### Selecting fields — `--col`, facets, `--all-cols`

Shared by `find` and `get`:

- **Bare names select frontmatter fields:** `--col status,title`.
- **Structural facets are dot-prefixed:** `.path`, `.stem`, `.frontmatter` (whole block), `.headings`, `.outgoing_links`, `.unresolved_links`, `.incoming_links`, `.body`, `.raw`.
- **`--all-cols`** dumps everything cache-served (frontmatter + every facet incl. `.body`), excluding `.raw` so a broad query never fans out to N file reads. Mutually exclusive with `--col`.
- `.body` is the parsed body from the cache; `.raw` is the file's exact bytes from disk.

### count

```bash
norn count                                    # total
norn count --eq type:note --by status         # grouped; same filters as find
norn count --path 'notes/**/*.md' --by type
```

`count` shares the full `find` filter surface. Formats: `text` (default) and `json` only.

### Output formats

`find`/`get` auto-detect by destination: TTY → `records`, pipe → `paths`. Override with `--format`.

- `records` — human-legible blocks. **Never parse it; not a stable contract.**
- `paths` — one vault-relative path per line. Stable.
- `json` — `find` emits one object: `{ total, returned, starts_at, documents[] }`. `get` emits a bare array of records (no wrapper). Stable, versioned.
- `jsonl` — one object per line, no wrapper. Stable; for streaming/early-close consumers.
- `markdown` — `get`-only, one document, rebuilt as Markdown.

Use `json` for one-shot dispatch, `jsonl` for queues. `paths`/`json`/`jsonl` never emit color.

## Links — relative Markdown first, wikilinks opt-in

norn is **link-syntax-neutral**. It treats relative Markdown links (`[label](../notes/foo.md)`) as the default idiom and Obsidian wikilinks (`[[target]]`) as a fully-supported opt-in form. Both participate equally in resolution, `find --links-to` / `--unresolved-links`, `get` link facets, `validate` link findings, and `move`/`delete` cascade rewrites. Lead with relative links unless the vault is Obsidian-flavored.

One asymmetry to know: `rewrite-wikilink` retargets wikilinks only. Relative Markdown links are rewritten automatically by `move`/`delete` when their target relocates.

## Mutation — safe by default

`new`, `set`, `edit`, `move`, `delete` are single-document, schema-aware writes. `migrate` applies a batch plan. All are safe-by-default, but the exact trigger differs — read the table before scripting them.

### The apply-model table

| Command | TTY, no flag | Non-TTY, no `--yes` | `--yes` | `--dry-run` | `--format json` |
|---|---|---|---|---|---|
| `new` | preview + confirm | dry-run (no write) | apply | preview | non-interactive, JSON report |
| `set` | preview + confirm | dry-run (no write) | apply | preview | non-interactive, `SetReport` |
| `edit` | preview + confirm | dry-run (no write) | apply | preview | non-interactive, `EditReport` |
| `move` | preview + confirm | dry-run (no write) | apply | preview | non-interactive, `ApplyReport` |
| `delete` | preview + confirm | dry-run (no write) | apply | preview | non-interactive, `ApplyReport` |
| `migrate` | confirm | applies (consumes a plan) | apply | preview | `ApplyReport` |
| `rewrite-wikilink` | confirm | applies | apply | preview | `ApplyReport` |

**Footgun:** for `new`/`set`/`move`/`delete`, running without `--yes` in a non-TTY context (i.e. from an agent) **writes nothing** — it dry-runs. Always pass `--yes` from an agent when you intend to apply.

### set

```bash
norn set notes/task.md --field status=active --yes
norn set notes/task.md --field status=active --dry-run     # preview only
norn set notes/task.md --push tags=work --yes              # append to a list
norn set notes/task.md --pop tags=old --yes               # remove from a list
norn set notes/task.md --remove priority --yes            # drop a key
norn set notes/task.md --field-json meta='{"n":1}' --yes  # structured value
echo "new body" | norn set notes/task.md --body-from-stdin --yes
```

Schema-aware: `field_types` validation runs before apply; `wikilink`-typed fields auto-wrap (`norn` → `[[norn]]`); a value outside `allowed_values` is refused (exit 2) unless `--force`; removing a required field needs `--force`. Exit codes: `0` ok/dry-run, `1` cancelled, `2` refusal.

### edit

`edit` makes surgical, content-anchored changes to a document's **body** — the rest stays byte-for-byte intact. Use it instead of `set --body-from-stdin` (wholesale body replacement) when you want to touch one phrase or one section. Frontmatter stays with `set`.

```bash
norn edit notes/task.md \
  --edits-json '[{"op":"str_replace","old":"draft","new":"final"}]' --yes
echo '[{"op":"append_to_section","heading":"Tasks","content":"- [ ] ship"}]' \
  | norn edit notes/task.md --yes      # array on stdin when --edits-json is absent
```

The edits are an **ordered JSON array** of ops, applied **all-or-nothing**: each op runs against the result of the prior, and any anchor failure refuses the whole batch with no partial write. Ops (tagged by `op`):

- `str_replace {old, new, replace_all?}` — literal replace; unique-match-or-refuse unless `replace_all`.
- `replace_section {heading, content}` — swap a section's body, heading kept.
- `append_to_section {heading, content}` — append to a section's body.
- `delete_section {heading}` — remove a heading and its body.
- `insert_before_heading` / `insert_after_heading {heading, content}` — positional insert.

Sections are addressed by **exact heading text**; a duplicated heading refuses as ambiguous (exit 2), and headings inside fenced code blocks are not matched. Exit codes: `0` ok/dry-run, `1` cancelled, `2` refusal. Output: `EditReport` (`schema_version: 1`).

**MCP (`vault.edit`):** the tool carries the identical op array, dry-run by default, `confirm: true` to write (refused under `--read-only`). For a non-idempotent op like `str_replace`, an MCP client must **read the dry-run response before sending `confirm: true`** — the confirm consumes the same anchor, so resending the op blind after it already applied would fail to re-match.

### new

```bash
norn new notes/my-note.md --yes                          # schema defaults fill required fields
norn new notes/my-note.md --field description="…" --yes  # override one field
norn new notes/my-note.md --dry-run                      # preview scaffold + defaults
echo "# Heading" | norn new notes/my-note.md --body-from-stdin --yes

# Rule-targeted: derive path from a named creatable rule
norn new --as task --title "Fix the cache" --var workspace=norn --yes
norn new --title "Quick capture" --yes                   # inbox fallback (inbox.path required)
```

`new` operates in three modes: (A) explicit path — supply the vault-relative path directly; (B) rule-targeted (`--as <rule>`) — derives the path from the named rule's `target` template, applies the rule's `frontmatter_defaults`, and seeds the body from its `body` scaffold; (C) inbox fallback — no path and no `--as`, routes to `inbox.path/<title|slugify>.md`. Template placeholders: `{{title}}`, `{{date}}`, `{{now}}`, `{{path.X}}`, `{{var.KEY}}` (filled by `--var KEY=VALUE`). `--field` overrides always win. Refuses (exit 2) when a required `{{var.KEY}}` is missing, `--title` is absent where the template needs it, the rule is unknown or non-creatable, or the inbox is unconfigured for Mode C. Also refuses if the path exists (unless `--force`) or a parent dir is missing (unless `-p`). After writing, `validate` runs against the new doc; findings surface as report warnings.

**MCP off-filesystem placement:** call `vault.describe` to inspect `creatable_rules` (each carries `name`, `target`, `required_vars`, `frontmatter_defaults`, `body`) and `inbox`. Then call `vault.new { rule: "task", title: "…", vars: { "workspace": "norn" }, confirm: true }` — norn derives the concrete path from the rule's template with no path guessing.

### move / delete

```bash
norn move inbox/task.md projects/task.md --yes           # rewrites both link syntaxes
norn move archive/ projects/ --recursive --yes
norn delete notes/old.md --rewrite-to notes/new.md --yes # redirect backlinks, then delete
norn delete notes/old.md --allow-broken-links --yes      # delete; let backlinks break
```

`move` rewrites every incoming link (relative + wikilink). `delete` refuses (exit 2) when the doc has incoming links unless `--rewrite-to <ALT>` or `--allow-broken-links` is given.

## Validate first, then repair, then migrate

### validate (read-only)

```bash
norn -C /path/to/vault validate --summary --format json   # size the work first
norn -C /path/to/vault validate --code 'link-*' --format jsonl
norn -C /path/to/vault validate --severity error --path 'notes/**' --format json
```

`--summary` returns grouped counts; run it before reading raw findings. Filters combine AND across types, OR within a type; `--code` and `--path` take globs. Formats: `records`, `jsonl` (pipe default — a finding has no path), `json` (`{ total, findings[] }`), `paths` (unique source paths). Exit code is `1` when any finding is an error, else `0` — gate pipelines on it.

Stable finding codes: `link-target-missing`, `link-anchor-missing`, `link-block-missing`, `link-ambiguous`, `frontmatter-required-field-missing`, `frontmatter-disallowed-value`, `frontmatter-invalid-type`, `frontmatter-forbidden-field`, `frontmatter-alias-shadowed-by-stem`, `frontmatter-alias-duplicate-across-docs`, `frontmatter-alias-malformed`, `document-misrouted`. Renames are CHANGELOG breaking changes.

### The plan/apply loop

```bash
# 1. detect + size
norn -C /vault validate --summary --format json

# 2. plan (read-only; never writes)
norn -C /vault repair --plan --code frontmatter-disallowed-value --field status --out plan.json

# 3. review plan.json — read summary.planned_changes and the skipped section

# 4. dry-run the apply (checks preconditions, writes nothing)
norn -C /vault migrate plan.json --dry-run --format json

# 5. apply
norn -C /vault migrate plan.json --yes --format json

# 6. re-validate as a follow-up step
norn -C /vault validate --summary --format json
```

Single-line pipeline (skips the artifact file): `norn -C /vault repair --plan --format json | norn -C /vault migrate - --yes`. `norn migrate -` and bare `norn migrate` both read the plan from stdin.

`migrate` is the batch write surface. It verifies the plan's vault root, re-reads each source doc and checks its recorded hash, and verifies each `expected_old_value` before writing — any precondition failure aborts the whole batch before any partial write. Re-plan rather than retrying; there is no `--force`. (There is no `--verify` flag — re-validate with a separate `norn validate` call as in step 6.)

### Repair plan shape

`repair --plan` formats: `report` (human, TTY default), `json` (full `MigrationPlan`, the only format `migrate` consumes; pipe default), `paths` (affected paths). Supported findings become `PlannedChange`s (path, field, new value, document hash). Skipped findings carry a stable reason code: `missing-default`, `link-decision-needed`, `no-rule-matched`, `alias-shadowed`, `graph-diagnostic`, `ambiguous-target`, `missing-hash`, `precondition-failed`. Filter with `--skip-reason <PATTERN>` (globs).

Repair-action kinds in a plan: `set_frontmatter`, `remove_frontmatter`, `add_frontmatter`, `move_document`, `rewrite_link`, `replace_body` (emitted only by `set --body-from-stdin`), `create_document` (emitted only by `new`). Closest-match `rewrite_link` proposals are confidence-banded (`high` = slug-identity, safe; `medium` = small edit distance, review). Use `--confidence high` to keep only high-confidence proposals. Ties skip with `ambiguous-target`; never auto-pick them.

## Audit trail

```bash
norn audit                                    # newest 20 events, records format
norn audit --trace a1b2c3d4                   # all events from one invocation
norn audit --status applied --limit 10        # applied mutations only
norn audit --target notes/my-note.md          # events touching a specific path
norn audit --since 2026-06-01 --until 2026-06-15  # UTC day range
norn audit --format json --limit 5            # stable JSON array
norn audit --raw --limit 5                    # stored OTEL Logs objects verbatim
```

`audit` reads the per-vault append-only mutation event stream. Only confirmed mutations are recorded (dry-runs and reads are not). An empty or absent stream returns `[]` with exit 0. Results are newest-first.

Filters (all AND-combined): `--trace <ID>` (one invocation), `--status applied|skipped|failed`, `--target <PATH>` (source or destination of a move), `--since`/`--until` (`YYYY-MM-DD` → UTC day bounds; RFC-3339 for precision), `--limit <N>` (default 20).

Output is a **flattened norn-native projection**: hot fields `trace`, `status`, `target`, and `target_to` promoted to top-level; remaining `norn.*` attributes in a generic `attributes` bag (prefix stripped, dots→underscores). `--raw` returns the stored OTEL Logs objects verbatim.

**MCP (`vault.audit`):** the tool carries the identical filter surface, returns `{ events: [...] }`, and — being read-only — is available even under `norn mcp --read-only`.

## User vault doctrine lives in .norn/config.yaml

Don't hardcode a vault's rule names, field shapes, or status vocabularies into prompts — read them from `<vault-root>/.norn/config.yaml`. It declares `files.ignore`, `validate.ignore`, `validate.required_frontmatter`, `validate.rules`, and `repair.rules`. No config → defaults apply. Inspect it with `norn config show`.

## Common pitfalls

- **Pass `--yes` from an agent.** A non-TTY mutation without `--yes` dry-runs and writes nothing.
- **Don't parse `records`.** It's for humans. Use `json`/`jsonl` (data) or `paths` (lists).
- **`--text` is substring, not search.** For frontmatter, use `--eq`/`--in`/etc.; for relevance ranking, norn is the wrong tool.
- **Don't auto-resolve `link-ambiguous`.** The `candidates` list is for a human or a documented disambiguation rule.
- **Re-plan, don't retry.** A stale-hash or mismatch abort means the vault changed; regenerate the plan.
- **Cache is disposable.** Query commands refresh it implicitly; if results look stale, `norn cache rebuild`. Treat cache errors as bugs, not retry states. `norn cache prune` evicts dead/aged cache entries across all vaults — run with `--dry-run` first.

## Reference and escape hatches

The skill is self-sufficient offline. For depth beyond it:

- `norn <command> --help` — authoritative, always-current flag reference (`-h` for the compact form). Offline.
- Per-command docs: https://github.com/dbtlr/norn/tree/main/docs/commands (moving to norn.run).
- Repository: https://github.com/dbtlr/norn
