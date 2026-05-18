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
      set_frontmatter:
        field: ...
        value: ...
      # or:
      remove_frontmatter:
        field: ...
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

Each rule has a `match` predicate and exactly one action (`set_frontmatter` or `remove_frontmatter`).

```yaml
repair:
  rules:
    - name: legacy-task-status-someday
      match:
        code: frontmatter-disallowed-value
        rule: task-status
        field: status
        actual_value: someday
      set_frontmatter:
        field: status
        value: backlog

    - name: remove-forbidden-kind
      match:
        code: frontmatter-forbidden-field
        field: kind
      remove_frontmatter:
        field: kind
```

`match` supports `code`, `rule`, `field`, and `actual_value`. Matches are exact and type-sensitive.

The first supported repair actions are frontmatter-only. Link rewriting and path moves are tracked for v0.27+.

## Examples

See [examples/config-minimal.yaml](../examples/config-minimal.yaml) and [examples/config-typed-notes.yaml](../examples/config-typed-notes.yaml) for runnable starting points.

## See also

- [Validate rule shape](rule-shape.md) — the selector + constraint conceptual model.
- [Validation and repair](validation.md) — finding codes and the apply contract.
- [Concepts](concepts.md) — glob semantics, lookup rules.
