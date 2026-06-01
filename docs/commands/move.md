---
title: move
description: Move or rename a document, rewriting incoming links across the vault.
---

# norn move

Move or rename a document and rewrite every incoming link across the vault — both relative Markdown links and wikilinks. `move` is the safe way to relocate a document without breaking the graph.

## Examples

```bash
norn move inbox/task.md projects/my-project/task.md
# preview + confirm in a TTY; backlinks rewritten

norn move inbox/task.md projects/my-project/task.md --dry-run
# preview the move and the backlink rewrites, no write

norn move inbox/task.md projects/my-project/task.md --yes --format json
# apply non-interactively; emit the ApplyReport

norn move inbox/task.md projects/my-project/ --parents
# move into a directory, creating missing parents

norn move archive/ projects/ --recursive
# move every .md under a directory in one cascade pass
```

## Link rewriting

By default `move` rewrites all incoming links so the graph stays intact:

- **Relative Markdown links** (`[label](../inbox/task.md)`) are recomputed to the new relative path.
- **Wikilinks** (`[[task]]`) are retargeted, preserving display text, anchor, and block-ref suffixes.

`--no-link-rewrite` moves the file only; incoming links are left to surface as broken in `norn validate`.

## Options

| Flag | Effect |
|---|---|
| `--dry-run` | Preview the move and rewrites; no write. |
| `--yes` | Apply without the confirm prompt. |
| `--no-link-rewrite` | Move the file but skip backlink rewriting. |
| `--force` | Overwrite the destination if it exists (otherwise refused, exit 2). |
| `-p`, `--parents` | Create missing destination parent directories. |
| `-r`, `--recursive` | Move every `.md` under a directory, preserving structure, in one cascade pass. |

## Apply model

Safe-by-default, like `set` / `new` / `delete`: a TTY previews and prompts; non-TTY without `--yes` is a dry-run; `--yes` applies; `--dry-run` previews and exits; `--format json` is non-interactive and emits the `ApplyReport` envelope.

Exit codes: `0` success or dry-run, `1` cancelled or runtime failure, `2` pre-flight refusal.

## Output

`--format records` (TTY summary) or `json` (the `ApplyReport`, listing `moved_files` and `rewritten_links`).

## See also

- [`delete`](delete.md) — remove a document, optionally redirecting its backlinks.
- [`rewrite-wikilink`](rewrite-wikilink.md) — retarget a wikilink without moving a file.
- [`validate`](validate.md) — surface broken links before or after a move.
- Run `norn move --help` for the full flag reference.
