---
title: Validate rule shape
description: The conceptual model for vault-cli validate rules — selectors that pick documents and constraints that check them, with worked examples.
---

# Validate Rule Shape

A validate rule has two parts: a **selector** that picks documents, and one
or more **constraints** that check them.

## Selectors

Selectors are ANDed. A rule fires on documents where every selector that is
present matches. Absent selectors are not constraints — they impose nothing.

- `match.path` — if present, the document path must match this glob
- `match.path_not` — if present, the document path must not match this glob
- `match.frontmatter` — if present and non-empty, every listed field must
  equal its declared value (exact, type-sensitive; missing fields do not
  match)
- `exclude.path` — if present, the document path must not match this glob.
  Equivalent to `match.path_not`, but named for clarity when carving out
  from a broader `match.path`.

A rule with no selectors fires on every non-ignored document. The top-level
`validate.required_frontmatter` is sugar for a single rule with no selectors
and only a `required_frontmatter` constraint.

## Constraints

Constraints are independent and additive. A single rule may declare any
combination; each constraint emits its own finding code when violated.
Constraints never interact — there is no rule-wide pass/fail, only finding
emissions.

| Constraint | Finding code | Fires when |
|---|---|---|
| `required_frontmatter` | `frontmatter-required-field-missing` | Listed field is absent or null |
| `forbidden_frontmatter` | `frontmatter-forbidden-field` | Listed field is present and non-null |
| `field_types` | `frontmatter-invalid-type` | Present value doesn't match declared shape |
| `allowed_values` | `frontmatter-disallowed-value` | Present value isn't one of the declared values |
| `allowed_paths` | `document-misrouted` | Document path matches no declared glob |

## Combining

A rule can declare any combination of constraints. For example:

```yaml
- name: agent-artifact-base
  match:
    frontmatter:
      type: agent-artifact
  forbidden_frontmatter: [kind]
  allowed_paths: ["Workspaces/**/agent-artifacts/*.md"]
  required_frontmatter: [artifact_kind]
```

This rule fires on any document with `type: agent-artifact` and emits up to
three independent findings: one for missing `artifact_kind`, one for present
`kind`, one for misrouted location.
