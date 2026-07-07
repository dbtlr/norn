---
title: Error and outcome contract
description: The exit-code contract, the machine-readable error-code taxonomy, and the apply outcome field — for agents and pipelines that branch on failure.
---

# Error and outcome contract

Mutation commands (`migrate`, `move`, `delete`, `rewrite-wikilink`) and their MCP
twins fail in structured, machine-branchable ways. This page is the contract an
agent or pipeline uses to decide **retry vs. re-read vs. give up** without
string-matching human prose.

Three things are stable and versioned (breaking changes are called out in the
CHANGELOG):

1. **Exit codes** — the process-level tri-state.
2. **The `outcome` field** — the same tri-state on the `ApplyReport`, exposed
   identically by the CLI (`--format json`) and the MCP `structuredContent`.
3. **Error codes** — a stable kebab-case vocabulary on the failure envelope and
   on a refused op's `error.code`.

## Exit codes

Mutation commands use a three-value process exit:

| Exit | Meaning | When |
|---|---|---|
| `0` | success | every op applied (or a dry-run forecast) |
| `1` | runtime op-failure | the apply began and at least one op failed |
| `2` | preflight / precondition refusal | a validation-phase check refused before any write; the vault is byte-identical |

Per-command exit meanings historically differed (a bad `--filter` on one command,
a schema refusal on another). The `outcome` field and the error-code taxonomy
below are the surface that reconciles them: **branch on `outcome` / `code`, not on
the exit integer alone.** Non-mutating commands may use exit `2` for usage/filter
errors; the tri-state above is the mutation-command contract.

## The `outcome` field

Every `ApplyReport` carries a single `outcome` field:

| `outcome` | Exit | `ApplyReport` shape |
|---|---|---|
| `applied` | 0 | ops `applied` / `skipped`; `failed == 0` |
| `failed` | 1 | at least one op `failed` after the apply began |
| `refused` | 2 | the offending op is `failed` with an `error`, the rest `not_run`; nothing written |
| `rebased` | 0 | **reserved** for a future auto-rebase-on-drift (NRN-152); not produced today |

`outcome` is the cross-surface signal. A consumer that ported "nonzero exit =
failure" logic should key on `outcome` instead: over MCP a **refused** apply is
returned as a normal `structuredContent` report (with `outcome: "refused"` and the
offending op `failed`), not as a transport error — so inspecting only a transport
`isError` flag would miss it.

### Return-report-on-refusal (MCP)

A **validation-phase precondition refusal** (see the taxonomy below) is returned to
an MCP caller as the `ApplyReport`:

- the offending op has `status: "failed"` and an `error` envelope,
- every other op is `not_run`,
- `outcome` is `refused`,
- the vault is byte-identical (nothing was written).

This lets a client distinguish **retryable CAS drift** (`stale-document-hash`,
`expected-old-value-mismatch` — re-read and re-plan) from a **terminal refusal** by
comparing `error.code`, never the message text. Post-write failures (which cannot
guarantee a byte-identical vault) still surface as a transport error carrying the
same structured `data` envelope.

## The error envelope

On the CLI, a `--format json` failure prints the envelope to **stdout** (prose still
goes to stderr for `records`/TTY output):

```json
{
  "code": "stale-document-hash",
  "message": "stale repair plan for a.md: expected hash …, found …; regenerate with `norn repair --plan`",
  "path": "a.md"
}
```

- `code` — a stable kebab-case identifier (below). Branch on this.
- `message` — human-readable prose. Do not parse it.
- `path` — the offending vault-relative path, when the failure is about one document.

Over MCP the same `{ code, message, path? }` envelope is carried in the JSON-RPC
error's `data` field (for post-write / non-refusal failures), and as a refused op's
`error` (for precondition refusals).

## Error-code taxonomy

All code values are canonically **kebab-case**. Codes are stable; a rename is a
CHANGELOG breaking change.

### Retryable — CAS drift (re-read and re-plan)

| Code | Cause |
|---|---|
| `stale-document-hash` | the document changed since the plan was generated |
| `expected-old-value-mismatch` | a field's expected old value no longer matches on disk |

### Terminal — plan / schema refusals

| Code | Cause |
|---|---|
| `unsupported-schema-version` | the plan's `schema_version` is not supported by this build |
| `vault-root-mismatch` | the plan's `vault_root` does not match the effective cwd |
| `unknown-path` | the plan targets a document not in the index |
| `conflicting-field-change` | two ops change the same field of the same document |
| `conflicting-hashes` | two ops assert divergent document hashes for one path |
| `unsupported-operation` | an unsupported repair operation for the target |
| `content-op-after-vacate` | a content edit follows a delete/move of the same path in one plan |

### Terminal — write-safety refusals

| Code | Cause |
|---|---|
| `cannot-minimal-edit` | frontmatter could not be minimally edited |
| `frontmatter-parse-failed` | the document's frontmatter did not parse |
| `post-image-verification-failed` | the post-edit frontmatter failed its read-back verification gate |
| `edit-failed` | a body/section edit op could not be applied |
| `field-already-present` | `add_frontmatter` refuses to overwrite an existing field (use `set`) |
| `missing-new-value` | a `create_document` / `set_frontmatter` op is missing its payload |

### Terminal — lifecycle preconditions

| Code | Cause |
|---|---|
| `move-source-missing` | the move source does not exist |
| `move-source-is-symlink` | the move source is a symlink, not a regular file |
| `move-destination-exists` | the move destination already exists |
| `delete-source-missing` | the delete source does not exist |
| `delete-source-is-symlink` | the delete source is a symlink, not a regular file |

### Terminal — vault containment

The vault is self-contained; a target that resolves outside the vault root is
refused.

| Code | Cause |
|---|---|
| `containment-absolute-path` | an absolute path is not vault-relative |
| `containment-parent-traversal` | a `..` component escapes the vault root |
| `containment-escapes-vault` | the path resolves outside the vault (symlink escape) |
| `containment-unresolvable` | vault-root containment could not be verified |

### Locking

| Code | Cause |
|---|---|
| `mutation-lock-timeout` | another norn mutation holds the per-vault lock |

### Fallback

| Code | Cause |
|---|---|
| `internal-error` | any failure without a more specific code |

## See also

- [Command reference](commands.md)
- [Agent workflows](agent-workflows.md) — the stable JSON/JSONL contracts.
- [migrate](commands/migrate.md) — apply a MigrationPlan with precondition checks.
