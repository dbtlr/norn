---
title: Validate rule shape
description: The conceptual model for norn validate rules — selectors that pick documents and constraints that check them, with worked examples.
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
  match its declared value. A scalar value means exact equality (type-
  sensitive; missing fields do not match). A list value means **any-of**:
  the field must equal one of the listed scalars — `type: [task, phase]`
  fires on either type. The list enumerates candidate values; it is not
  containment over an array-valued field.
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
| `field_references` | `frontmatter-reference-type` | A field's wikilink resolves to a document whose `type` is outside the allowed set |

### Typed references — `field_references`

`field_references` is the typed half of referential integrity: link validation
already checks that a wikilink *resolves*; this constraint checks that the
target is the right *kind* of document.

```yaml
- name: task
  match:
    frontmatter:
      type: task
  field_references:
    parent:
      target_type: [phase, initiative]   # any-of, like match.frontmatter
    depends_on:
      target_type: task                  # scalar = one-element set
```

The check judges only **resolved** frontmatter wikilinks in the named field
(scalar or array element alike): unresolved and ambiguous references stay
link validation's findings (`link-*`), never a reference-type violation. A
resolved target without a `type` field is outside every allowed set and
reports as `(missing)`. The constraint is validate-time only — `norn set`
does not resolve references at write time.

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

An any-of selector lets one **base rule** carry constraints shared across
several types, with per-type rules refining on top — no duplicated
constraints and no synthetic discriminator field:

```yaml
- name: node-base
  match:
    frontmatter:
      type: [task, phase, initiative]
  required_frontmatter: [id, title, parent]
- name: task
  match:
    frontmatter:
      type: task
  required_frontmatter: [lifecycle]
```

## Creation defaults

In addition to constraints (validated at `norn validate` time), a rule can
declare `frontmatter_defaults` — values that `norn new` fills in when
creating a new document whose path matches the rule. Defaults complement
constraints: a rule can require `status` AND declare `status: backlog` as
the default, so `norn new` produces valid documents without operator
intervention.

Substitution language and transforms apply to default values. See
`docs/configuration.md` for the full vocabulary.
