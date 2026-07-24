---
title: validate
description: Validate vault graph facts and configured frontmatter rules.
---

# norn validate

Validate the vault against the rules you declare in `.norn/config.yaml` — required fields, allowed values, expected types, path scoping — plus graph facts like broken and ambiguous links. `validate` never writes; it reports findings with stable codes. Pipelines gate on its exit code: `1` when any finding is an error, `0` otherwise.

## Examples

```bash
norn validate
# human-readable findings on the configured vault

norn validate --format json
# machine-readable findings for pipelines

norn validate --summary --format json
# grouped finding counts — size the work before reading it

norn validate --severity error
# errors only; skip warnings

norn validate --code 'link-*'
# broken and ambiguous links (glob match)

norn validate --code 'link-*' --format paths
# unique source paths only; pipe-friendly

norn validate --path 'notes/**' --format jsonl
# scope validation to a subtree
```

## Summary first

`--summary` emits grouped counts (by code, severity, field, and more) instead of raw findings. Run it first to size the work, then re-run without `--summary` to read individual findings. The same filters apply to both.

## Triage filters

Filters combine with AND across types and OR within a type (comma-separated). Glob patterns work on `--code` and `--path`.

| Filter | Matches |
|---|---|
| `--code <CODE>` | Finding code. Comma-separated = any; globs like `link-*` work. |
| `--severity <SEVERITY>` | `error` or `warning`. |
| `--field <FIELD>` | Frontmatter field name. |
| `--rule <RULE>` | Validate rule name. |
| `--path <GLOB>` | Vault-relative path glob. |
| `--target <TARGET>` | Link target string. |
| `--reason <REASON>` | Unresolved-link reason. |

## Finding codes

Codes are stable; renames are called out as breaking changes in the CHANGELOG.

| Code | Meaning |
|---|---|
| `read-failed` | The document could not be read from disk. |
| `frontmatter-unclosed` | Frontmatter `---` opener has no closing `---`. |
| `frontmatter-parse-failed` | YAML frontmatter could not be parsed. |
| `frontmatter-json-conversion-failed` | Parsed YAML frontmatter could not be converted to JSON. |
| `link-target-missing` | A link target doesn't exist in the vault. |
| `link-anchor-missing` | The target exists but the `#anchor` isn't present. |
| `link-block-missing` | The target exists but the `^block-ref` isn't present. |
| `link-ambiguous` | A link resolves to multiple candidates. |
| `frontmatter-required-field-missing` | A required field is absent. |
| `value-not-allowed` | A field's value is not in the configured set. |
| `field-type-invalid` | A field's value doesn't match its declared type. |
| `frontmatter-exceeds-max-length` | A field's value exceeds its effective max length. |
| `frontmatter-forbidden-field` | A field the rule forbids is present. |
| `frontmatter-reference-type` | A frontmatter wikilink resolves to a document of a disallowed `type`. |
| `document-misrouted` | A document is in a directory its rule's path selector excludes. |

All codes above are `warning` severity except `read-failed`, which is `error`. See [Validation and repair](../validation.md#finding-codes) for the fields each code carries.

## Output formats

`records` (TTY), `jsonl` (pipe default — a finding has no natural path representation), `json` (`{ total, findings[] }`), and `paths` (unique source paths). `json` / `jsonl` / `paths` never carry color.

## See also

- [Validation and repair](../validation.md) — finding-code semantics, the summary shape, and repair recipes.
- [`repair`](repair.md) — turn supported findings into a fixable plan.
- Run `norn validate --help` for the full flag reference.
