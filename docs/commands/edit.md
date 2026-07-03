---
title: edit
description: Edit one document's body with atomic, content-anchored partial edits.
---

# norn edit

Edit one document's body with surgical, content-anchored partial edits. `edit` takes an ordered JSON array of ops and applies them all-or-nothing — each op runs against the result of the prior, and any anchor failure refuses the whole batch with no partial write.

Use `edit` for targeted changes that keep the rest of the body byte-for-byte intact. For a wholesale body rewrite, use [`set --body-from-stdin`](set.md); for frontmatter, use [`set`](set.md).

## Examples

```bash
norn edit notes/project.md \
  --edits-json '[{"op":"str_replace","old":"draft","new":"final"}]' --yes
# literal replace; refuses unless "draft" matches exactly once

echo '[{"op":"str_replace","old":"draft","new":"final"}]' \
  | norn edit notes/project.md --yes
# same edit, array read from stdin

norn edit notes/project.md \
  --edits-json '[{"op":"append_to_section","heading":"Tasks","content":"- [ ] ship it"}]' --yes
# append a line to the body under the "Tasks" heading

echo '[{"op":"append_to_section","heading":"Tasks","content":"- [ ] ship it"}]' \
  | norn edit notes/project.md --dry-run
# preview the append without writing
```

The edits array comes from `--edits-json` or, when that flag is absent, from stdin. An empty or malformed array is refused (exit 2) before any lock or cache work.

## Operations

Each op is a JSON object tagged by `op`. Headings are addressed by exact text.

| Op | Fields | Effect |
|---|---|---|
| `str_replace` | `old`, `new`, `replace_all?` | Replace literal `old` with `new`. Unique-match-or-refuse unless `replace_all` is `true`. |
| `replace_section` | `heading`, `content` | Replace a section's body, keeping the heading line. |
| `append_to_section` | `heading`, `content` | Append `content` to the end of a section's body. |
| `delete_section` | `heading` | Remove a heading line and its body. |
| `insert_before_heading` | `heading`, `content` | Insert `content` immediately before a heading line. |
| `insert_after_heading` | `heading`, `content` | Insert `content` immediately after a heading line, before its body. |

A heading that appears more than once is **ambiguous** and refuses the batch (exit 2) — disambiguate by editing surrounding text with `str_replace` first, or rename one heading. Headings inside fenced code blocks are not matched. Ops apply sequentially, so a later op can anchor on text an earlier op produced.

## When to use `edit` vs `set`

- **`norn edit`** — surgical changes to a document's body: replace a phrase, swap one section, append a checklist item, drop a stale section. The rest of the body is untouched.
- **`norn set --body-from-stdin`** — replace the *entire* body wholesale. Reach for it when you have the full new body in hand and don't need anchoring.
- **`norn set`** — frontmatter fields. `edit` never touches frontmatter.

## Apply model

`edit` is destructive and safe-by-default, matching `set`:

- **TTY:** shows a preview and prompts for confirmation.
- **Non-TTY without `--yes`:** prints the preview and exits — the preview is your dry-run. Nothing is written.
- **`--yes`:** applies without prompting.
- **`--dry-run`:** previews and exits explicitly.
- **`--format json`:** non-interactive; emits the `EditReport` envelope.

Apply is atomic — the whole batch resolves to one filesystem write, audited to the event stream with a `trace_id`.

## Guarding against concurrent drift (`--expected-hash`)

By default `edit` is read-modify-write: it reads the current body, applies the ops, and writes — the one-shot common case. When you read a document, decide on an edit, and want to be sure nothing changed underneath you in between, pass `--expected-hash <HASH>`:

```bash
hash=$(norn get notes/project.md --col .document_hash --format json | jq -r '.[0].document_hash')
# ...decide on an edit against what you just read...
norn edit notes/project.md \
  --edits-json '[{"op":"str_replace","old":"draft","new":"final"}]' \
  --expected-hash "$hash" --yes
# refuses (exit 2, no write) if the document drifted from $hash since you read it
```

`HASH` is the blake3 hex of the document's **full** content (frontmatter + body) — the same `document_hash` value plan ops carry. Read it with `get --col .document_hash` (or `find --col .document_hash` for many docs); an MCP client reads it the same way via `vault.get`. Read it on the **default** path (not `--no-cache-refresh`): `.document_hash` is served from the cache, which the pre-query refresh reconciles with disk — under `--no-cache-refresh` the reported hash can lag on-disk content, and `edit` then recomputes the fresh hash and refuses with a spurious drift error. The check runs in preflight, before the transform, so a stale hash refuses even under `--dry-run` rather than previewing a phantom edit. Omit the flag and `edit` behaves exactly as before. The MCP `vault.edit` tool takes the same precondition as an `expected_hash` argument.

Exit codes: `0` success or dry-run, `1` operator-cancelled, `2` pre-flight refusal (malformed/empty array, string not found or ambiguous, heading not found or ambiguous, or `--expected-hash` drift).

## Output

`--format records` (TTY) or `json` (the `EditReport` envelope, `schema_version: 1`).

## See also

- [`set`](set.md) — frontmatter mutation and wholesale body replacement.
- [`get`](get.md) — inspect a document's body and headings before editing it.
- Run `norn edit --help` for the full flag reference.
