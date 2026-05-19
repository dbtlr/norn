---
title: Configuration
description: The .vault/config.yaml schema covering file ignores, validate rules, and repair rules, with worked examples.
---

# Configuration

`vault` looks for `.vault/config.yaml` at the root of the effective vault directory (the `-C` path, the `--vault <name>` registered path, or the process cwd if neither is set). Missing config is fine — defaults apply.

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
        field: datetime | date | list_of_strings | wikilink | wikilink_or_list
      allowed_values:
        field: [value1, value2]
      allowed_paths:
        - "..."
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

Ignored targets stay out of the graph entirely. If an indexed document links to an ignored file, that link is reported as `link-unresolved` rather than silently hidden.

## validate.ignore

Path globs that remain in the graph but are skipped by `vault validate`. Use this for content you want indexed (so links resolve correctly) but don't want to assert standards against.

```yaml
validate:
  ignore:
    - "archive/**"
    - "templates/**"
```

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
- `match.frontmatter` — top-level scalar equality (exact, type-sensitive; missing fields do not match).
- `exclude.path` — equivalent to `match.path_not`, named for carving out from a broader `match.path`.

Constraints (independent and additive):

| Constraint | Finding code | Fires when |
|---|---|---|
| `required_frontmatter` | `frontmatter-required-field-missing` | Listed field is absent or null. |
| `forbidden_frontmatter` | `frontmatter-forbidden-field` | Listed field is present and non-null. |
| `field_types` | `frontmatter-invalid-type` | Present value doesn't match declared shape. |
| `allowed_values` | `frontmatter-disallowed-value` | Present value isn't one of the declared values. |
| `allowed_paths` | `document-misrouted` | Document path matches no declared glob. |

Supported `field_types`: `datetime`, `date`, `list_of_strings`, `wikilink`, `wikilink_or_list`. Field-type checks only run when the field is present — combine with `required_frontmatter` when presence is also required.

`datetime` accepts ISO/YAML forms with optional seconds, fractional seconds, `Z`, numeric timezone offsets, or a space separator. `date` accepts plain `YYYY-MM-DD` values and YAML-normalized midnight datetime strings.

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

## repair.rules

Declarative deterministic repair rules. `vault repair plan` matches findings against `repair.rules` and converts matched findings into executable changes; unmatched findings appear in `skipped_findings` with `skip_reason: unsupported`.

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

**Known v0.28.0 limitation:** when a backlinking file contains multiple identical link occurrences pointing at the moved file, only the first occurrence is rewritten. Subsequent identical raw occurrences remain unchanged; running `vault validate` after apply will flag them as unresolved.

A rename whose new stem already exists elsewhere produces a non-blocking `StemCollisionAfterMove` warning attached to the planned change.

## Examples

See [examples/config-minimal.yaml](../examples/config-minimal.yaml) and [examples/config-typed-notes.yaml](../examples/config-typed-notes.yaml) for runnable starting points.

## See also

- [Validate rule shape](rule-shape.md) — the selector + constraint conceptual model.
- [Validation and repair](validation.md) — finding codes and the apply contract.
- [Concepts](concepts.md) — glob semantics, lookup rules.
