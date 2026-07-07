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
| `1` | partial-apply failure | at least one filesystem write had already landed when a later op failed — the vault is **partially mutated** |
| `2` | preflight / precondition refusal | a check refused before **any** write; the vault is byte-identical |

The `1`-vs-`2` split is decided by a single runtime fact: **did any filesystem
write land before the failure?** It is *not* decided by the error variant. The
same code (e.g. `stale-document-hash`, `unknown-path`) can surface as a byte-
identical refusal (exit 2) when it fires before the first write, or as a partial
failure (exit 1) when it fires after an earlier op in the same plan already wrote.
Branch on `outcome` / the report, not on the code alone.

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
| `failed` | 1 | a write already landed, then an op failed — already-applied ops `applied`, the failing op `failed` with an `error`, the rest `not_run`; the vault is **partially mutated** |
| `refused` | 2 | the offending op is `failed` with an `error`, the rest `not_run`; **nothing written**, vault byte-identical |
| `rebased` | 0 | **reserved** for a future auto-rebase-on-drift (NRN-152); not produced today |

`outcome` is the cross-surface signal, and the `failed`-vs-`refused` distinction is
load-bearing: `refused` promises a byte-identical vault (safe to retry or ignore),
while `failed` means **the vault was partially mutated** — a consumer must re-read
before retrying. A consumer that ported "nonzero exit = failure" logic should key
on `outcome` instead: over MCP both a `refused` and a `failed` apply are returned as
a normal `structuredContent` report (not a transport error), so inspecting only a
transport `isError` flag would miss both.

### Return-report on the MCP surface

A **clean refusal** (nothing written) is returned to an MCP caller as the
`ApplyReport`:

- the offending op has `status: "failed"` and an `error` envelope,
- every other op is `not_run`,
- `outcome` is `refused`,
- the vault is byte-identical (nothing was written).

A **partial apply** (a write landed, then a later op failed) is *also* returned as
the `ApplyReport`, but with the truthful partial state:

- every op that completed has `status: "applied"`,
- the failing op has `status: "failed"` with its `error` envelope,
- ops that never ran are `not_run`,
- `outcome` is `failed` — the vault is **partially mutated**, so re-read before retrying.

Either way a client distinguishes **retryable CAS drift** (`stale-document-hash`,
`expected-old-value-mismatch` — re-read and re-plan) from a **terminal refusal** by
comparing `error.code`, never the message text — and reads `outcome` to learn
whether the vault was touched.

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

Over MCP the same `{ code, message, path? }` envelope is carried as the failing
op's `error` inside the returned `ApplyReport` — for both a `refused` apply
(nothing written) and a `failed` apply (partial mutation). A structurally invalid
request (an unparseable plan, a bad `schema_version`) still surfaces as a transport
error carrying the envelope in the JSON-RPC error's `data` field.

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
