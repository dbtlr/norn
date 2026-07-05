---
title: Configuration
description: The .norn/config.yaml schema covering file ignores, validate rules, repair rules, and cache lifecycle settings, with worked examples.
---

# Configuration

Config is discovered relative to `--cwd` (or `$NORN_ROOT`, else `$PWD`, if unset). `norn` looks for `.norn/config.yaml` at that root; missing config is fine — defaults apply.

Pass `--config <path>` to point at an explicit file. Relative paths resolve against the effective cwd.

## Schema overview

```yaml
files:
  ignore:           # path globs excluded from the graph entirely
    - "..."
validate:
  ignore:           # path globs visible in the graph but skipped by validate
    - "..."
  required_frontmatter:    # global presence requirement (sugar for a no-selector rule)
    - title
  rules:            # scoped validate rules; see rule-shape.md
    - name: rule-name
      match:
        path: "..."
        path_not: "..."
        frontmatter:
          field: value
      exclude:
        path: "..."
      required_frontmatter: [...]
      forbidden_frontmatter: [...]
      field_types:
        field: datetime | date | list_of_strings | wikilink | wikilink_or_list | string | text
        field2: { type: string, max_length: 32, indexed: true }   # extended form
        field3: { indexed: true }   # type-less: index vote only, no type declared
      allowed_values:
        field: [value1, value2]
      allowed_paths:
        - "..."
index:
  auto: true        # auto-index bounded-type fields (default true)
repair:
  rules:            # deterministic repair rules; see validation.md
    - name: rule-name
      match:
        code: finding-code
        rule: rule-name
        field: frontmatter-field
        actual_value: ...
      # exactly one action per rule:
      set_frontmatter:
        field: ...
        value: ...
      # or:
      remove_frontmatter:
        field: ...
      # or:
      add_frontmatter:
        field: ...
        value: ...
      # or:
      move_document:
        to_directory: ...   # OR to_path: ...
cache:
  retention: 90d    # age-eviction window for cache prune (default 90d)
  prune: lazy       # lazy (default) | manual
```

## files.ignore

Path globs excluded from the graph before file inventory and document parsing. With no config, the graph is a raw filesystem view except for hidden files and directories.

```yaml
files:
  ignore:
    - "node_modules/**"
    - ".obsidian/**"
    - "target/**"
    - "**/*.tmp"
```

Ignored targets stay out of the graph entirely. If an indexed document links to an ignored file, that link is reported as `link-target-missing` rather than silently hidden.

Patterns support literal path segments, `*` (matches within a single path segment), and `**` (matches across segments) — e.g. `Archive/**`, `**/*.tmp`, `Workspaces/*/drafts/**`. Richer glob syntax accepted by `validate.ignore` (`?`, `[...]` character classes, `{a,b}` alternation) is **not** interpreted here and matches literally.

`files.ignore` is applied at cache-build time: a change takes effect on the next cache build or refresh (the daemon reopens automatically). A query run with `--no-cache-refresh` sees the cache as last built, so a just-added ignore entry is not reflected until a refresh runs.

## validate.ignore

Path globs that remain in the graph but are skipped by `norn validate`. This is **tier 2** of the ignore model — where `files.ignore` removes a document from the graph entirely (tier 1), `validate.ignore` keeps it fully in the graph and only exempts it from standards enforcement.

```yaml
validate:
  ignore:
    - "archive/**"
    - "templates/**"
```

A `validate.ignore`'d document is:

- **Indexed and queryable.** It counts in `norn count`, is returned by `norn find`, and `norn get` reads its frontmatter — indexing is independent of validation. Use this for content you want discoverable and link-resolvable but don't want to assert standards against.
- **A valid link target.** Links into it resolve normally (it is not `link-target-missing`), and links it makes out are followed.
- **Exempt from every validate finding.** Both schema findings (missing/disallowed frontmatter, type violations) and `link-target-missing` for its own outgoing links are suppressed.
- **Allowed to have malformed or absent frontmatter.** A `validate.ignore`'d document need not parse as valid YAML — the `frontmatter-parse-failed` finding is suppressed along with the rest. (A malformed document is still skipped by frontmatter-dependent queries rather than crashing them.)

Unlike `files.ignore` (literal path segments only), `validate.ignore` accepts the richer glob syntax — `?`, `[...]` character classes, and `{a,b}` alternation — in addition to `*` and `**`.

## validate.required_frontmatter

Sugar for a single rule with no selectors and only a `required_frontmatter` constraint. Applies to every document not skipped by `validate.ignore`.

```yaml
validate:
  required_frontmatter:
    - title
```

## validate.rules

Scoped rules with selectors and constraints. See [rule-shape.md](rule-shape.md) for the conceptual model.

Selectors (all ANDed):

- `match.path` — vault-relative path glob.
- `match.path_not` — exclude matching paths.
- `match.frontmatter` — top-level scalar equality (exact, type-sensitive; missing fields do not match). A list value is an **any-of** selector: `type: [task, phase]` fires the rule when the field equals any listed scalar (candidate values, not array containment). An empty list is a config error.
- `exclude.path` — equivalent to `match.path_not`, named for carving out from a broader `match.path`.

Constraints (independent and additive):

| Constraint | Finding code | Fires when |
|---|---|---|
| `required_frontmatter` | `frontmatter-required-field-missing` | Listed field is absent or null. |
| `forbidden_frontmatter` | `frontmatter-forbidden-field` | Listed field is present and non-null. |
| `field_types` | `frontmatter-invalid-type` | Present value doesn't match declared shape. |
| `field_types` (`max_length`) | `frontmatter-exceeds-max-length` | Present `string`/`list_of_strings` value matches its declared type but exceeds the effective `max_length` bound. |
| `allowed_values` | `frontmatter-disallowed-value` | Present value isn't one of the declared values. |
| `allowed_paths` | `document-misrouted` | Document path matches no declared glob. |
| `field_references` | `frontmatter-reference-type` | A field's wikilink resolves to a document whose `type` is outside the declared `target_type` set. |

`field_references` declares typed references per field: `field_references: { parent: { target_type: [phase, initiative] } }` (scalar or any-of list, like `match.frontmatter`). Only resolved frontmatter wikilinks are judged — broken references stay `link-*` findings; a resolved target without `type` reports as `(missing)`. Validate-time only.

Supported `field_types`: `datetime`, `date`, `list_of_strings`, `wikilink`, `wikilink_or_list`, `string`, `text`. Field-type checks only run when the field is present — combine with `required_frontmatter` when presence is also required.

- `frontmatter_defaults`: map of field → default value. Used by `norn new` to
  fill required fields when creating a new document. Values may use the
  substitution language (`{{title}}`, `{{date}}`, `{{time}}`, `{{date:fmt}}`,
  `{{time:fmt}}`, `{{now}}`, `{{path.X}}`) and pipe transforms
  (`titlecase`, `sentencecase`, `lower`, `upper`, `unsep`, `strip_date_prefix`,
  `slugify`). See `CHANGELOG.md` for details.

`datetime` accepts ISO/YAML forms with optional seconds, fractional seconds, `Z`, numeric timezone offsets, or a space separator. `date` accepts plain `YYYY-MM-DD` values and YAML-normalized midnight datetime strings.

`string` is a bounded scalar: at most `max_length` characters, default 64, raisable to a 256-char ceiling. `text` is its unbounded counterpart — any length, no `max_length` allowed. `list_of_strings` elements are bounded the same way as `string` (default 64, same 256 ceiling), each element checked independently. An over-length `string`/`list_of_strings` value reports `frontmatter-exceeds-max-length` rather than `frontmatter-invalid-type`.

### Extended `field_types` form

Alongside the bare `field: type_name` shorthand, a field can declare the extended object form:

```yaml
field_types:
  project: { type: string, max_length: 32, indexed: true }
  notes: { type: text }
  status: { indexed: false }   # type-less: index vote only
```

- `type` — the field's type, same vocabulary as the bare form.
- `max_length` — only valid on `string`/`list_of_strings`; must be between 1 and 256; defaults to 64 when the type is bounded and `max_length` is omitted.
- `indexed` — forces the field in (`true`) or out (`false`) of the derived frontmatter index, overriding auto-indexing (see `index.auto` below). Valid on any type.

`type` may be omitted only when `indexed` is the entry's sole key (`{ indexed: true }` / `{ indexed: false }`) — a type-less entry votes on indexing only and declares no type: `validate`, `norn set`, and `norn new` all treat the field as undeclared for type-checking and coercion purposes. Any other type-less combination (e.g. `{ max_length: 32 }` alone, or `{ indexed: false, max_length: 5 }`) is a config load error, as is an unknown key or a `max_length` on a type that carries no length bound.

### Worked example

```yaml
validate:
  rules:
    - name: typed-note
      match:
        path: "**/*.md"
        frontmatter:
          type: note
      required_frontmatter:
        - kind
      field_types:
        created: datetime
        modified: datetime
        tags: list_of_strings

    - name: task-status
      match:
        path: "**/*.md"
        frontmatter:
          type: task
      required_frontmatter:
        - status
      allowed_values:
        status:
          - backlog
          - in_progress
          - completed
          - wont_do
      allowed_paths:
        - "tasks/**/*.md"
```

Findings include `rule` context when a scoped rule produced them.

## index

Config for the derived frontmatter index (used to accelerate queries):

```yaml
index:
  auto: true   # default true
```

`auto` (default `true`) controls automatic indexing: when true, every field with a bounded `field_types` type (`date`, `datetime`, `wikilink`, `wikilink_or_list`, `string`, `list_of_strings`) or an `allowed_values` constraint is indexed for query acceleration, unless an explicit `indexed: false` overrides it. `text` fields — the one unbounded scalar type — are never auto-indexed; give a `text` field `indexed: true` explicitly to include it anyway. Set `auto: false` to index only fields with an explicit `indexed: true`.

## repair.rules

Declarative deterministic repair rules. `norn repair --plan` matches findings against `repair.rules` and converts matched findings into executable changes; unmatched findings appear in `skipped_findings` with `skip_reason: no_rule_matched`.

Each rule has a `match` predicate and exactly one action (`set_frontmatter`, `remove_frontmatter`, `add_frontmatter`, or `move_document`).

`match` supports `code`, `rule`, `field`, and `actual_value`. Matches are exact and type-sensitive.

### set_frontmatter

Replace an existing frontmatter field's value. Apply preserves byte-for-byte the surrounding YAML (comments, ordering, quote style); only the value of the matched field changes.

```yaml
- name: legacy-task-status-someday
  match:
    code: frontmatter-disallowed-value
    rule: task-status
    field: status
    actual_value: someday
  set_frontmatter:
    field: status
    value: backlog
```

### remove_frontmatter

Remove a frontmatter field entirely.

```yaml
- name: remove-forbidden-kind
  match:
    code: frontmatter-forbidden-field
    field: kind
  remove_frontmatter:
    field: kind
```

### add_frontmatter

Insert a missing frontmatter field. Refuses at apply time if the field is already present (use `set_frontmatter` for replacement).

```yaml
- name: ensure-research-kind
  match:
    code: frontmatter-required-field-missing
    rule: typed-note
    field: kind
  add_frontmatter:
    field: kind
    value: research
```

### move_document

Move or rename a file. Accepts `to_directory` (file moves into the directory, filename preserved) OR `to_path` (full destination including filename; handles renames).

```yaml
# Move into a directory, preserving filename
- name: route-tasks-dir
  match:
    code: document-misrouted
    rule: task-routing
  move_document:
    to_directory: "Workspaces/{frontmatter.workspace}/tasks/"

# Full destination, including possible rename
- name: route-tasks-path
  match:
    code: document-misrouted
    rule: task-routing
  move_document:
    to_path: "Workspaces/{frontmatter.workspace}/tasks/{stem}.md"
```

Either form supports placeholder substitution:

- `{stem}` — the source file's stem (filename without extension).
- `{filename}` — the source file's filename including extension.
- `{frontmatter.<field>}` — a scalar value from the source file's frontmatter.

If substitution fails (missing field, non-scalar value), the finding is skipped with `skip_reason: precondition_failed`.

Apply automatically rewrites backlinks alongside the move:

- Stem-only wikilinks `[[task]]` rewrite when the stem changes.
- Path-qualified wikilinks `[[Inbox/task]]` rewrite when the path changes.
- Markdown links `[text](path)` rewrite when the path changes.

**Known v0.28.0 limitation:** when a backlinking file contains multiple identical link occurrences pointing at the moved file, only the first occurrence is rewritten. Subsequent identical raw occurrences remain unchanged; running `norn validate` after apply will flag them as unresolved.

A rename whose new stem already exists elsewhere produces a non-blocking `StemCollisionAfterMove` warning attached to the planned change.

## cache

Cache-lifecycle settings for [`norn cache prune`](commands/cache.md#pruning) and the per-invocation lazy sweep. Both keys are optional; absent means the defaults below.

```yaml
cache:
  retention: 90d    # age-eviction window (default 90d)
  prune: lazy       # lazy (default) | manual
```

- `retention` — how old a vault's cache entry may grow (newest-file-mtime metric) before the prune sweep evicts it. Duration strings: `<n>w`, `<n>d`, `<n>h`, `<n>m` (e.g. `90d`, `12w`, `24h`). Parsing is best-effort: a malformed value is ignored and the 90d default applies; config load never fails on it. The `--retention` flag on `norn cache prune` overrides this value.
- `prune` — `lazy` (default) runs the cross-vault prune sweep on norn invocations, at most once per 24h; `manual` disables the automatic sweep for vaults run with this config (the explicit `norn cache prune` always works). An unknown value warns on stderr and defaults to `lazy`.

## Templates

Optional. Overrides defaults for the substitution language's `{{date}}` and
`{{time}}` vars used in `frontmatter_defaults`.

```yaml
templates:
  date_format: "YYYY-MM-DD"  # default
  time_format: "HH:mm"       # default
```

Format strings follow Moment.js-subset tokens: YYYY, YY, MM, M, MMM, MMMM, DD,
D, HH, H, hh, h, mm, ss, A, a, dddd, ddd. See `CHANGELOG.md` for the full
substitution-language reference.

## Examples

See [examples/config-minimal.yaml](../examples/config-minimal.yaml) and [examples/config-typed-notes.yaml](../examples/config-typed-notes.yaml) for runnable starting points.

## See also

- [Validate rule shape](rule-shape.md) — the selector + constraint conceptual model.
- [Validation and repair](validation.md) — finding codes and the apply contract.
- [Concepts](concepts.md) — glob semantics, lookup rules.
