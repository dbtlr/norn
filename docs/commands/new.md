---
title: new
description: Create a document with frontmatter pre-filled from the path's schema rules.
---

# norn new

Create a new Markdown document with frontmatter pre-filled from the schema rules that match its path. `new` infers required-field defaults from the matching rule, applies substitution variables (date, time, title, path), and writes the document atomically. Operator `--field` overrides always win over schema defaults.

## Examples

```bash
norn new notes/my-note.md --yes
# schema defaults fill the required frontmatter

norn new notes/my-note.md --field description="Design pass" --yes
# override one field; the rest come from the matched rule

norn new inbox/draft.md --parents --yes
# -p / --parents creates missing ancestor directories

norn new notes/my-note.md --dry-run
# preview the scaffold and defaults without writing

echo "# Heading" | norn new notes/my-note.md --body-from-stdin --yes
# supply body content alongside the scaffolded frontmatter
```

## Frontmatter pre-fill

`new` matches the new document's path against the configured rules and fills `frontmatter_defaults` from the matching rule, running the substitution language over each value (`{{title}}`, `{{date}}`, `{{now}}`, `{{path.X}}`, and the full transform set). `--field` / `--field-json` overrides take precedence over any default.

## Options

| Flag | Effect |
|---|---|
| `--field KEY=VALUE` | Override or add a frontmatter field. Repeatable. |
| `--field-json KEY=JSON` | Override a field with a raw JSON value. |
| `--body-from-stdin` | Read the document body from stdin. |
| `-p`, `--parents` | Create missing ancestor directories. |
| `--force` | Overwrite an existing destination and skip schema-aware coercion. |

`new` refuses if the path exists (unless `--force`) or a parent directory is missing (unless `-p`).

## Apply model

Same safe-by-default model as `set`, `move`, and `delete`:

- **TTY:** preview and confirm.
- **Non-TTY without `--yes`:** dry-run; nothing written.
- **`--yes`:** apply.
- **`--dry-run`:** preview and exit.
- **`--format json`:** non-interactive; emits the JSON report (`operation`, `frontmatter_created`, `warnings`, `trace_id`).

After writing, `norn validate` runs against the new document; findings surface as report warnings.

## Output

`--format records` (TTY) or `json`.

## See also

- [`set`](set.md) — edit a document after creating it.
- [`init`](init.md) — scaffold the `.norn/config.yaml` whose rules drive the pre-fill.
- [`validate`](validate.md) — the rules `new` checks against post-write.
- Run `norn new --help` for the full flag reference.
