---
title: delete
description: Delete a document, optionally redirecting its incoming links.
---

# norn delete

Delete a document. `delete` refuses (exit 2) when the document has incoming links, unless you acknowledge the breakage or redirect the links to an alternate target — so backlinks are never silently stranded.

## Examples

```bash
norn delete notes/old-note.md --dry-run
# preview; reports incoming links that would break

norn delete notes/old-note.md --rewrite-to notes/replacement.md --yes
# redirect every incoming link to a replacement, then delete

norn delete notes/old-note.md --allow-broken-links --yes
# delete and let the backlinks surface as broken in validate
```

## Incoming links

If the target has incoming links, `delete` refuses unless one of:

- **`--rewrite-to <ALT>`** — redirect every incoming link to `<ALT>` before deleting. Mutually exclusive with `--allow-broken-links`.
- **`--allow-broken-links`** — delete anyway; the broken links surface as `link-target-missing` findings in `norn validate`.

A document with no incoming links deletes without either flag.

## Apply model

Safe-by-default, like `set` / `new` / `move`: a TTY previews and prompts; non-TTY without `--yes` is a dry-run; `--yes` applies; `--dry-run` previews and exits; `--format json` is non-interactive and emits the `ApplyReport` envelope.

Exit codes: `0` success or dry-run, `1` cancelled or runtime failure, `2` pre-flight refusal.

## Output

`--format records` (TTY summary) or `json` (the `ApplyReport`).

## See also

- [`move`](move.md) — relocate a document instead of deleting it.
- [`find`](find.md) — `norn find --links-to notes/old-note.md` lists what links to a document before you delete it.
- Run `norn delete --help` for the full flag reference.
