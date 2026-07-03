---
title: new
description: Create a document in three modes — explicit path, rule-targeted (--as), or inbox fallback.
---

# norn new

Create a new Markdown document with frontmatter pre-filled from the schema rules. `new` operates in three modes, applies the substitution language over field values (`{{title}}`, `{{date}}`, `{{now}}`, `{{path.X}}`, `{{var.X}}`, and pipe transforms), and writes the document atomically. Operator `--field` overrides always win over schema defaults.

## Three creation modes

### Mode A — explicit path

Supply the vault-relative path directly. norn matches the path against configured rules and fills `frontmatter_defaults` from the matching rule.

```bash
norn new notes/my-note.md --yes
norn new notes/my-note.md --field description="Design pass" --yes
```

### Mode B — rule-targeted (`--as`)

Supply a creatable rule name via `--as`. norn derives the path from the rule's `target` template, applies the rule's `frontmatter_defaults`, and seeds the body from the rule's `body` scaffold (if declared). A rule is creatable when it declares both `name:` and `target:`.

```bash
norn new --as task --title "Audit the cache" --var workspace=norn --yes
norn new --as task --title "Audit the cache" --var workspace=norn --dry-run
```

The `--title` value is available as `{{title}}` (and `{{title|slugify}}`) in the template. Template variables declared in the `target` template as `{{var.KEY}}` must be supplied via `--var KEY=VALUE`.

### Mode C — inbox fallback

Supply `--title` without a path or `--as`. The new document is placed under the configured `inbox.path` as `<inbox>/<title|slugify>.md`.

```bash
norn new --title "Quick capture" --yes
```

Requires `inbox.path` to be set in `.norn/config.yaml`. Refuses (exit 2) when the inbox is unconfigured.

## Examples

```bash
# Mode A: explicit path
norn new notes/my-note.md --yes

# Mode B: rule-targeted
norn new --as task --title "Fix the query planner" --var workspace=norn --yes
norn new --as task --title "Fix the query planner" --var workspace=norn --dry-run

# Mode C: inbox fallback
norn new --title "Meeting notes 2026-06-23" --yes

# Common flags
norn new notes/my-note.md --parents --yes          # create missing ancestor dirs
norn new notes/my-note.md --dry-run                # preview without writing
echo "# Heading" | norn new notes/my-note.md --body-from-stdin --yes
```

## Config keys

### `validate.rules[].target` (makes a rule creatable)

A path template string. Presence of `target:` (combined with `name:`) makes the rule creatable via `--as`. Mutually exclusive with `match.path` on the same rule.

```yaml
validate:
  rules:
    - name: task
      target: "Workspaces/{{var.workspace}}/tasks/{{title|slugify}}.md"
      body: "## Context\n\n## Notes\n"
      frontmatter_defaults:
        type: task
        status: backlog
```

Template placeholders: `{{title}}`, `{{title|slugify}}`, `{{date}}`, `{{now}}`, `{{var.KEY}}` (filled by `--var KEY=VALUE`), `{{seq}}` (auto-incrementing id, see below), and any transform the substitution engine supports (`|slugify`, `|lower`, etc.).

#### `{{seq}}` — auto-incrementing ids

A `target` may include a `{{seq}}` token to allocate the next integer id at creation time, e.g. `target: "tasks/MMR-{{seq}}.md"`. The id is `max + 1` over the existing files sharing the resolved prefix, so it is **scoped per prefix** — `MMR-{{seq}}` counts only `MMR-*` files (independent of `NRN-*`), and the first id is `1`. Allocation happens at apply time under the per-vault mutation lock, so two concurrent creations get distinct, sequential ids with no collision.

On `--dry-run`, the reported `path` keeps the unresolved `{{seq}}` template and a separate `predicted_path` field shows the id that *would* be allocated — non-binding, since a concurrent creation could take it first. Ids are derived from the files on disk, not a stored counter: deleting the highest-numbered file frees its id for reuse on the next creation, while deleting a lower one leaves the next id unchanged.

Ids are plain, unpadded integers (`1`, `2`, … `10`). `{{seq}}` has no zero-padding directive, so don't point a `{{seq}}` rule at a directory that already uses zero-padded ids (`task-007.md`) — the next id would be `task-8.md`, breaking the lexical sort the padding existed to preserve. The token must appear exactly once, in the file name; a `{{seq}}` in a directory component (or twice) is refused at plan time.

### `validate.rules[].body` (body scaffold)

An optional inline body template seeded into the new document. Rendered with the same substitution context used for the path (so `{{title}}` and `{{var.X}}` work). Overridden by `--body-from-stdin`.

### `inbox:` (default target for Mode C)

```yaml
inbox:
  path: Inbox
```

`inbox.path` is the vault-relative directory for unrouted creates. When set, `norn new --title "..."` (no path, no `--as`) routes the document there.

## Options

| Flag | Effect |
|---|---|
| `--as RULE` | Create into a named creatable rule; derives the path from its `target` template. |
| `--title TEXT` | Document title — fills `{{title}}` in templates; required for Mode B (when the template needs it) and Mode C (inbox). |
| `--var KEY=VALUE` | Template variable, repeatable. Fills `{{var.KEY}}` in the `target` and `body` templates. |
| `--field KEY=VALUE` | Override or add a frontmatter field. Repeatable. |
| `--field-json KEY=JSON` | Override a field with a raw JSON value. |
| `--body-from-stdin` | Read the document body from stdin (takes precedence over the rule's `body` scaffold). |
| `-p`, `--parents` | Create missing ancestor directories. |
| `--force` | Overwrite an existing destination and skip schema-aware coercion. |

## Refusals (exit 2)

- Both path and `--as` supplied.
- `--as` names an unknown rule, or a rule with no `target` (non-creatable).
- A `{{var.KEY}}` in the template has no matching `--var KEY=VALUE`.
- `--title` is missing and the template or inbox mode requires it.
- Mode C (inbox fallback) and no `inbox.path` is configured.
- Malformed `--var` (no `=` separator).
- Path exists (unless `--force`) or parent directory missing (unless `-p`).

## Apply model

Same safe-by-default model as `set`, `move`, and `delete`:

- **TTY:** preview and confirm.
- **Non-TTY without `--yes`:** dry-run; nothing written.
- **`--yes`:** apply.
- **`--dry-run`:** preview and exit.
- **`--format json`:** non-interactive; emits the JSON report (`operation`, `path`, `applied`, `frontmatter_created`, `body_bytes`, `warnings`, `trace_id`).

After writing, `norn validate` runs against the new document; findings surface as report warnings.

## Output

`--format records` (TTY default) or `json`.

## See also

- [`set`](set.md) — edit a document after creating it.
- [`init`](init.md) — scaffold the `.norn/config.yaml` whose rules drive the pre-fill.
- [`validate`](validate.md) — the rules `new` checks against post-write.
- Run `norn new --help` for the full flag reference.
