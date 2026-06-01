---
title: set
description: Update one document — schema-aware frontmatter mutation and wholesale body replacement.
---

# norn set

Update one document: mutate frontmatter fields and optionally replace the body. `set` runs schema-aware validation against the configured `field_types`, then applies every op as a single atomic write.

## Examples

```bash
norn set notes/project.md --field status=active --yes
# set a field; skip the confirm prompt

norn set notes/project.md --field status=active --dry-run
# preview the change without writing

norn set notes/project.md --push aliases=new-alias --yes
# append to an array-typed field

norn set notes/project.md --pop aliases=old-name --yes
# remove a value from an array (silent if absent)

norn set notes/project.md --remove priority --yes
# drop a frontmatter key

norn set notes/project.md --field-json tags='["foo","bar"]' --yes
# set a structured value via raw JSON

echo "new body" | norn set notes/project.md --body-from-stdin --yes
# replace the body wholesale; frontmatter untouched
```

## Operations

| Flag | Effect |
|---|---|
| `--field KEY=VALUE` | Set a frontmatter field. Repeatable; repeats of the same key accumulate into an array. |
| `--field-json KEY=JSON` | Set a field to an explicit JSON value — arrays, nested objects, `null`. |
| `--push KEY=VALUE` | Append to a list-typed field; creates a single-element array if the key is absent. |
| `--pop KEY=VALUE` | Remove a value from a list-typed field. Silent no-op if absent. |
| `--remove KEY` | Drop a frontmatter key entirely. Silent no-op if absent. |
| `--body-from-stdin` | Replace the body wholesale with stdin. Frontmatter is kept. |

All ops in one invocation apply as a single filesystem write — any pre-flight refusal aborts the whole set with no partial write.

## Schema awareness

When `field_types` rules are configured, `set` validates each value's type before applying. A field typed `wikilink` or `wikilink_or_list` auto-wraps a bare value on write (`norn` becomes `[[norn]]`); unresolved or ambiguous targets surface as warnings, not refusals. A value outside a field's `allowed_values` is refused (exit 2) unless `--force`. `--remove` on a required field is refused unless `--force`. `--force` bypasses type validation and required-field protection.

## Apply model

`set` is destructive and safe-by-default:

- **TTY:** shows a preview and prompts for confirmation.
- **Non-TTY without `--yes`:** prints the preview and exits — the preview is your dry-run. Nothing is written.
- **`--yes`:** applies without prompting.
- **`--dry-run`:** previews and exits explicitly.
- **`--format json`:** non-interactive; emits the `SetReport` envelope.

Exit codes: `0` success or dry-run, `1` operator-cancelled, `2` pre-flight refusal.

## Output

`--format records` (TTY) or `json` (the `SetReport` envelope, `schema_version: 1`).

## See also

- [`get`](get.md) — inspect a document before editing it.
- [`new`](new.md) — create a document with schema-filled frontmatter.
- [`validate`](validate.md) — find the drift that `set` fixes.
- Run `norn set --help` for the full flag reference.
