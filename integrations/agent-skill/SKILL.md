---
name: norn
description: Use when inspecting, querying, validating, or mutating Markdown vaults with the `norn` CLI. Provides deterministic graph, link, frontmatter, query, and validation/repair workflows.
version: 1.4.0
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

## Discover the vault before creating or mutating

Once the vault root is picked, orient before querying or writing anything — don't guess at folder layout, rule names, or the frontmatter schema.

```bash
norn describe --format json    # the orient-first move
```

`describe` returns `folders` (every directory currently holding a document), `path_rules` (each rule's `match.path` glob plus the `frontmatter_defaults` a document there inherits), `creatable_rules` (rules usable with `norn new --as <rule>`: `name`, `target` template, `required_vars`, `frontmatter_defaults`, optional `body` scaffold), `inbox` (the `inbox.path` fallback target for `norn new --title "…"` with no path/`--as`, or `null`), and `schema` (the full `validate` config verbatim — every rule's `required_frontmatter`, `field_types`, `allowed_values`). Add `--data`/`--stats` for a contents-summary (`total`, per-field value distributions, date bounds) over the same filter surface `find`/`count` share — `--by field1,field2` names exact fields (bypassing the auto identity-skip that drops near-unique fields like `title`), and `--limit N` caps shown value-buckets per field (default 20, `0` = no cap). Read-only; never writes.

No `.norn/config.yaml` yet? `norn init` scaffolds one with commented example rules (refuses to overwrite an existing config unless `--force`). Once it exists:

```bash
norn config show       # effective config: resolved paths, counts
norn config validate   # check the config file itself for errors
norn config edit       # open it in $VISUAL/$EDITOR (auto-validates after)
norn config migrate    # upgrade an older config to the current schema version
```

`config show`/`validate` are read-only; `config edit`/`migrate` write to `.norn/config.yaml` itself (not vault documents). `describe` reads that config back as `path_rules`/`creatable_rules`/`inbox`/`schema` — after editing rules, re-run `describe` to confirm what an agent now sees.

## Query and read — the everyday surface

`find` selects a *set* of documents by predicate; `get` selects *named* documents by identity. They share one output contract: the same `--col` vocabulary, the same formats, the same sort/paging. Learn it once.

### find

```bash
norn find --eq type:note --limit 5            # frontmatter equality; find defaults to 10
norn find --text "reorg" --format paths       # case-insensitive body substring
norn find --has aliases --col title,aliases   # narrow the fields shown
norn find --in type:note,log --sort modified --desc
norn find --starts-with tags:release: --col title,tags   # enumerate a tag namespace
norn find --links-to notes/my-note.md --format paths
norn find --unresolved-links --format paths   # documents with broken links
norn find --all --all-cols --format jsonl     # whole-vault structured dump
```

Predicates (all ANDed; comma-separated values inside `--in`/`--not-in` are ORed): `--text`, `--eq`, `--not-eq`, `--in`, `--not-in`, `--starts-with`, `--ends-with`, `--contains` (anchored string operators on a frontmatter field or its array elements; case-sensitive, literal — `--contains` is frontmatter-scoped, body substring is `--text`), `--has`, `--missing`, `--before`, `--after`, `--on` (accepts `today`), `--path` (glob), `--links-to`, `--unresolved-links`. Every value-comparing predicate matches array elements (any element satisfies; negations require none to); string and date values collapse `[[wikilink]]` brackets on both sides, and number/bool values compare typed and the rule is symmetric — `--eq n:5` matches `5`, `5.0`, or `[5]` but never `"5"`, and a string value never matches numeric/boolean storage. A bare `norn find` with no predicate prints its help — pass `--all` to dump the whole vault on purpose.

### get

```bash
norn get notes/my-note.md                     # frontmatter + headings + links
norn get "My Note"                            # resolve by stem (case-insensitive)
norn get a.md b.md --col title,status         # several docs, narrowed
norn get notes/my-note.md --col .incoming_links
norn get notes/my-note.md --all-cols --format json
norn get notes/my-note.md --format markdown   # rebuild the doc as Markdown (get-only)
norn get notes/my-note.md --section "Task Description" --section "Annotations" --format json
                                               # named sections' content, repeat per heading (get-only)
```

A target is a path, a unique stem, or a wikilink-shaped string. `get` returns every named target (no default limit), unlike `find`.

`--section` reads named sections of the body — a distinct flag, not a `--col` facet, so it combines freely with `--col`/`--all-cols`. It is **repeatable**: pass it once per heading (`--section "A" --section "B"`), and each occurrence is one whole string, so a heading containing a comma (`--section "Risks, Open Questions"`) is addressable verbatim. Each heading resolves with the same boundary semantics `edit --append-to-section`/`--replace-section` use (heading line through the next same-or-higher heading, or EOF): a section read mirrors a section write, and the section content is byte-identical across formats. `--format json`/`jsonl` add a `sections` object keyed by heading text (unordered/alphabetical lookup); `records` prints one block per requested section in request order; `paths`/`markdown` ignore it entirely (no resolution, still exits 0), like `--col`. A heading missing or ambiguous in a given document warns and is omitted from that document's `sections` without affecting siblings or other targets — unless **none** of the requested headings resolve for that document, in which case the target hard-fails (nonzero exit) instead of returning an empty `sections` object.

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
norn count --by project,lifecycle             # multi-key: nested distribution per project
norn count --path 'notes/**/*.md' --by type
```

`count` shares the full `find` filter surface. `--by` takes one or more comma-separated fields: one field → flat value→count `groups` with a string `by`; several → nested groups (one map level per field, counts at the leaves) with an array `by`. Formats: `text` (default) and `json` only.

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

`new`, `set`, `edit`, `move`, `delete` are single-document, schema-aware writes. `apply` applies a batch plan. All are safe-by-default, but the exact trigger differs — read the table before scripting them.

### The apply-model table

| Command | TTY, no flag | Non-TTY, no `--yes` | `--yes` | `--dry-run` | `--format json` |
|---|---|---|---|---|---|
| `new` | preview + confirm | dry-run (no write) | apply | preview | non-interactive, JSON report |
| `set` | preview + confirm | dry-run (no write) | apply | preview | non-interactive, `SetReport` |
| `edit` | preview + confirm | dry-run (no write) | apply | preview | non-interactive, `EditReport` |
| `move` | preview + confirm | dry-run (no write) | apply | preview | non-interactive, `ApplyReport` |
| `delete` | preview + confirm | dry-run (no write) | apply | preview | non-interactive, `ApplyReport` |
| `apply` | confirm | dry-run (no write) | apply | preview | **applies without `--yes`**, `ApplyReport` |
| `rewrite-wikilink` | confirm | dry-run (no write) | apply | preview | **applies without `--yes`**, `ApplyReport` |

**Footgun:** for every command in this table, running without `--yes` in a non-TTY context (i.e. from an agent) **writes nothing** — it dry-runs, same as `--dry-run`, even though the exit code is 0. Always pass `--yes` from an agent when you intend to apply. **One exception:** `apply` and `rewrite-wikilink` treat `--format json` as consent to apply — with `--format json` and no `--yes` they **write**, even in a non-TTY context (`set`/`delete` and the rest do not; JSON output there is still an implicit dry-run). Don't lean on that divergence: pass `--yes` when you mean to apply and `--dry-run` when you don't.

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

`new` operates in three modes: (A) explicit path — supply the vault-relative path directly; (B) rule-targeted (`--as <rule>`) — derives the path from the named rule's `target` template, applies the rule's `frontmatter_defaults`, and seeds the body from its `body` scaffold; (C) inbox fallback — no path and no `--as`, routes to `inbox.path/<title|slugify>.md`. Template placeholders: `{{title}}`, `{{date}}`, `{{now}}`, `{{path.X}}`, `{{var.KEY}}` (filled by `--var KEY=VALUE`), `{{seq}}` (auto-incrementing id — see below). `--field` overrides always win. Refuses (exit 2) when a required `{{var.KEY}}` is missing, `--title` is absent where the template needs it, the rule is unknown or non-creatable, or the inbox is unconfigured for Mode C. Also refuses if the path exists (unless `--force`) or a parent dir is missing (unless `-p`). After writing, `validate` runs against the new doc; findings surface as report warnings.

#### `{{seq}}` — auto-incrementing ids, no hand-rolled next-id logic

A rule's `target` template can include `{{seq}}` instead of an agent computing the next id itself (e.g. by `find`-ing existing files and counting):

```yaml
validate:
  rules:
    - name: task
      target: "tasks/NRN-{{seq}}.md"
      frontmatter_defaults:
        type: task
        status: backlog
```

```bash
norn new --as task --title "Fix the query planner" --yes
# → tasks/NRN-<next-id>.md, id = max existing NRN-* file + 1 (first is 1)
```

The id is allocated at apply time under the per-vault mutation lock, so concurrent creates never collide, and it's scoped per resolved prefix (`NRN-{{seq}}` counts only `NRN-*` files, independent of any other `{{seq}}`-templated rule). Ids are plain, unpadded integers — don't point a `{{seq}}` rule at a directory using zero-padded ids (`task-007.md`); the next id would be `task-8.md`, breaking lexical sort. On `--dry-run`, the reported `path` keeps the literal `{{seq}}` token and a separate `predicted_path` shows the id that would be allocated (non-binding — a concurrent create could take it first).

**MCP off-filesystem placement:** call `vault.describe` to inspect `creatable_rules` (each carries `name`, `target`, `required_vars`, `frontmatter_defaults`, `body`) and `inbox`. Then call `vault.new { rule: "task", title: "…", vars: { "workspace": "norn" }, confirm: true }` — norn derives the concrete path from the rule's template with no path guessing.

### move / delete

```bash
norn move inbox/task.md projects/task.md --yes           # rewrites both link syntaxes
norn move archive/ projects/ --recursive --yes
norn delete notes/old.md --rewrite-to notes/new.md --yes # redirect backlinks, then delete
norn delete notes/old.md --allow-broken-links --yes      # delete; let backlinks break
```

`move` rewrites every incoming link (relative + wikilink). `delete` refuses (exit 2) when the doc has incoming links unless `--rewrite-to <ALT>` or `--allow-broken-links` is given.

## Validate first, then repair, then apply

### validate (read-only)

```bash
norn -C /path/to/vault validate --summary --format json   # size the work first
norn -C /path/to/vault validate --code 'link-*' --format jsonl
norn -C /path/to/vault validate --severity error --path 'notes/**' --format json
```

`--summary` returns grouped counts; run it before reading raw findings. Filters combine AND across types, OR within a type; `--code` and `--path` take globs. Formats: `records`, `jsonl` (pipe default — a finding has no path), `json` (`{ total, findings[] }`), `paths` (unique source paths). Exit code reflects **whole-vault** error-severity diagnostics — it does not change with `--code`/`--severity`/`--path`, and most finding codes default to `warning` severity. Don't gate a pipeline on exit code alone; check `--summary` totals or the returned findings instead.

Stable finding codes (18): `read-failed`, `frontmatter-unclosed`, `frontmatter-parse-failed`, `frontmatter-json-conversion-failed`, `link-target-missing`, `link-anchor-missing`, `link-block-missing`, `link-ambiguous`, `frontmatter-required-field-missing`, `frontmatter-forbidden-field`, `frontmatter-invalid-type`, `frontmatter-exceeds-max-length`, `frontmatter-disallowed-value`, `document-misrouted`, `frontmatter-reference-type`, `frontmatter-alias-malformed`, `frontmatter-alias-shadowed-by-stem`, `frontmatter-alias-duplicate-across-docs`. See [validation.md](https://github.com/dbtlr/norn/tree/main/docs/validation.md) for severity and source per code. Renames are CHANGELOG breaking changes.

### The plan/apply loop

```bash
# 1. detect + size
norn -C /vault validate --summary --format json

# 2. plan (read-only; never writes)
norn -C /vault repair --plan --code frontmatter-disallowed-value --field status --out plan.json

# 3. review plan.json — read summary.planned_changes and the skipped section

# 4. dry-run the apply (checks preconditions, writes nothing)
norn -C /vault apply plan.json --dry-run --format json

# 5. apply
norn -C /vault apply plan.json --yes --format json

# 6. re-validate as a follow-up step
norn -C /vault validate --summary --format json
```

Single-line pipeline (skips the artifact file): `norn -C /vault repair --plan --format json | norn -C /vault apply - --yes`. `norn apply -` and bare `norn apply` both read the plan from stdin.

`apply` is the batch write surface. It verifies the plan's vault root, re-reads each source doc and checks its recorded hash, and verifies each `expected_old_value` before writing — any precondition failure aborts the whole batch before any partial write. Re-plan rather than retrying; there is no `--force`. (There is no `--verify` flag — re-validate with a separate `norn validate` call as in step 6.)

### Repair plan shape

`repair --plan` formats: `report` (human, TTY default), `json` (full `MigrationPlan`, the only format `apply` consumes; pipe default), `paths` (affected paths). Supported findings become `PlannedChange`s (path, field, new value, document hash). Skipped findings carry a stable reason code: `missing-default`, `link-decision-needed`, `no-rule-matched`, `alias-shadowed`, `graph-diagnostic`, `ambiguous-target`, `missing-hash`, `precondition-failed`. Filter with `--skip-reason <PATTERN>` (globs).

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

Don't hardcode a vault's rule names, field shapes, or status vocabularies into prompts — read them from `<vault-root>/.norn/config.yaml`. It declares `files.ignore`, `validate.ignore`, `validate.required_frontmatter`, `validate.rules`, and `repair.rules`. No config → defaults apply. The two `ignore` keys are a two-tier model: `files.ignore` removes a document from the graph entirely (not indexed, not queryable, not a link target), while `validate.ignore` keeps it fully indexed and queryable (`find`/`get`/`count` still see it, links still resolve) and only exempts it from `validate` findings — including a pass for malformed or absent frontmatter. Inspect it with `norn config show`. `field_types` entries with a bounded type (`string`, `date`, `datetime`, `wikilink`, `wikilink_or_list`, `list_of_strings`) or an explicit `indexed: true` get shredded into a derived index (`index.auto`, default on) that `find`/`count` route through automatically — no query-side flag needed, it's purely a config-time performance lever.

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
