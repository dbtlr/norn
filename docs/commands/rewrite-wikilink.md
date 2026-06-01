---
title: rewrite-wikilink
description: Rewrite every occurrence of a wikilink target across the vault.
---

# norn rewrite-wikilink

Rewrite every occurrence of a wikilink target across the vault — in both document bodies (`[[OLD]]`, `[[OLD|display]]`) and frontmatter fields holding the target as a wikilink value. Use it to retarget a `[[wikilink]]` when the file itself hasn't moved.

> Relative Markdown links are rewritten automatically by [`move`](move.md) and [`delete`](delete.md) when their target relocates. `rewrite-wikilink` covers the wikilink case where no file moved; a syntax-neutral standalone retarget for relative links is not yet a separate command.

## Examples

```bash
norn rewrite-wikilink old-name new-name --dry-run
# preview every occurrence that would change

norn rewrite-wikilink old-name new-name --yes
# apply the rewrite across the vault

norn rewrite-wikilink old-name new-name --yes --format json
# apply and emit the ApplyReport
```

## Behavior

`OLD` and `NEW` are wikilink targets (stem, path, or alias). `rewrite-wikilink` builds a one-op `MigrationPlan` and runs it through the shared applier, preserving display text, anchor, and block-ref suffixes on each rewritten link. It refuses (exit 2) when `OLD` doesn't resolve to any document.

## Options

| Flag | Effect |
|---|---|
| `--dry-run` | Preview changes without writing. |
| `--yes` | Skip the TTY confirmation and apply. |
| `--out <PATH>` | Write the JSON `ApplyReport` to a file. |
| `--format records\|json` | Output shape. |

Exit codes: `0` success or dry-run, `1` runtime failure, `2` pre-flight refusal.

## See also

- [`move`](move.md) — relocate a file and rewrite all incoming links (both syntaxes).
- [`delete`](delete.md) — redirect backlinks with `--rewrite-to`.
- Run `norn rewrite-wikilink --help` for the full flag reference.
