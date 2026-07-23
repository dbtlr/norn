---
title: Error and outcome contract
description: The exit-code contract, the machine-readable error-code taxonomy, and the apply outcome field — for agents and pipelines that branch on failure.
---

# Error and outcome contract

Mutation commands (`apply`, `move`, `delete`, `rewrite-wikilink`) and their MCP
twins fail in structured, machine-branchable ways. This page is the contract an
agent or pipeline uses to decide **retry vs. re-read vs. give up** without
string-matching human prose.

Three things are stable and versioned (breaking changes are called out in the
CHANGELOG):

1. **Exit codes** — the process-level tri-state.
2. **The `outcome` field** — the same tri-state on the `ApplyReport`, exposed
   identically by the CLI (`--format json`) and the MCP `structuredContent`.
3. **Error codes** — a stable kebab-case vocabulary on the failure envelope and
   on a refused operation or first-class precondition's `error.code`.

## Exit codes

Mutation commands use a three-value process exit:

| Exit | Meaning | When |
|---|---|---|
| `0` | success | every op applied (or a dry-run forecast) |
| `1` | partial-apply failure | at least one filesystem write had already landed when a later op failed — the vault is **partially mutated** |
| `2` | preflight / precondition refusal | a check refused before **any** write; the vault is untouched |

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
| `failed` | 1 | a write already landed, then an op failed — already-applied ops `applied`, the failing op `failed` with an `error`, the rest `not-run`; the vault is **partially mutated** |
| `refused` | 2 | an operation is `failed`, or a first-class precondition is `failed` while every operation is `not-run`; **nothing written**, vault untouched |
| `rebased` | 0 | **reserved** for a future auto-rebase-on-drift (NRN-152); not produced today |

`outcome` is the cross-surface signal, and the `failed`-vs-`refused` distinction is
load-bearing: `refused` promises nothing was written (safe to retry or ignore),
while `failed` means **the vault was partially mutated** — a consumer must re-read
before retrying. A consumer that ported "nonzero exit = failure" logic should key
on `outcome` instead: over MCP both a `refused` and a `failed` apply are returned as
a normal `structuredContent` report (not a transport error), so inspecting only a
transport `isError` flag would miss both.

### Return-report on the MCP surface

A **clean refusal** (nothing written) is returned to an MCP caller as the
`ApplyReport`:

- the offending operation has `status: "failed"` and an `error` envelope, or a first-class precondition has `status: "failed"` and an error while every operation is `not-run`,
- `outcome` is `refused`,
- the vault is untouched — nothing was written.

A **partial apply** (a write landed, then a later op failed) is *also* returned as
the `ApplyReport`, but with the truthful partial state:

- every op that completed has `status: "applied"`,
- the failing op has `status: "failed"` with its `error` envelope,
- ops that never ran are `not-run`,
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
operation or precondition's `error` inside the returned `ApplyReport` — for both a `refused` apply
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
| `vault-root-unreadable` | the vault root fails to canonicalize at apply time — a missing root or a non-searchable parent directory. A root that itself canonicalizes but has unreadable *contents* is not this: it currently surfaces as an owner cache-build failure, not an apply-time refusal |
| `unknown-path` | the plan targets a document not in the index |
| `conflicting-field-change` | two ops change the same field of the same document |
| `conflicting-hashes` | two ops assert divergent document hashes for one path |
| `unsupported-operation` | an unsupported repair operation for the target |
| `content-op-after-vacate` | a content edit follows a delete/move of the same path in one plan |
| `owner-set-mismatch` | the current paths owning a logical stem or exact frontmatter identity differ from the plan's expected set |
| `owner-claim-conflict` | two creates in one plan claim the same operation-derived logical stem |

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

### Terminal — `set` schema / argument refusals

`norn set` / `vault.set`'s schema-aware validation and `KEY=VALUE` argument
parsing (NRN-221). `frontmatter-parse-failed` reuses the write-safety code
above — a `set` target whose on-disk frontmatter fails to parse hits the same
underlying condition, so it is not a separate code.

| Code | Cause |
|---|---|
| `field-type-invalid` | a value does not coerce to its field's declared schema type (datetime/date/wikilink shape, or a `--field-json` type mismatch) — fix the value and retry. Shared with `norn validate`, which emits the same code for the same schema condition on an existing on-disk value |
| `field-type-unsupported` | the vault's schema declares a `field_type` this norn build does not support — a config defect; fix the schema, not the value |
| `value-too-long` | a `string` / `list_of_strings` value exceeds the field's `max_length` |
| `value-not-allowed` | a value is outside the field's `allowed_values` set. Shared with `norn validate`, which emits the same code for the same schema condition on an existing on-disk value |
| `field-json-invalid` | a `--field-json` value is not valid JSON |
| `required-field-removed` | `--remove` targets a `required_frontmatter` field |
| `target-not-found` | the `DOC` argument does not resolve to any document |
| `target-ambiguous` | the `DOC` argument resolves to more than one document |
| `assignment-malformed` | a `KEY=VALUE` argument is missing its separator or has an empty key |
| `field-conflict` | the same key is targeted by more than one of `--field`/`--field-json`/`--push`/`--pop`/`--remove` |
| `push-on-scalar` | `--push` targets a key whose current value is a scalar, not an array |
| `frontmatter-not-mapping` | a document's frontmatter parses to a non-mapping JSON value |

### Terminal — `move` / `delete` / `rewrite-wikilink` preflight refusals

`norn move` / `delete` / `rewrite-wikilink` and their `vault.*` MCP tools
(NRN-229). These fire at PREFLIGHT — when an argument does not resolve against
the graph index, or a policy blocks the mutation — before any plan is applied,
so the vault is byte-identical. They reuse `set`'s `target-*` and `new`'s
destination codes wherever the semantic is identical (one-semantic-one-code).

Distinct from the apply-time `move-source-missing` / `delete-source-missing`
lifecycle codes above: those fire inside the applier when the file is
missing/present on the FILESYSTEM at apply time; these fire earlier, at
argument resolution.

| Code | Cause |
|---|---|
| `target-not-found` | the move SOURCE / delete target / rewrite OLD does not resolve to any document (shared with `set`) |
| `target-ambiguous` | the move SOURCE / delete target resolves to more than one document (shared with `set`) |
| `destination-exists` | the move destination already exists; pass `--force` to overwrite (shared with `new`) |
| `parent-missing` | the move destination's parent directory does not exist; pass `-p` / `--parents` (shared with `new`) |
| `source-destination-same` | the move source and destination resolve to the same canonical path (a no-op) |
| `backlinks-present` | the delete target has incoming links; pass `--allow-broken-links` or `--rewrite-to <ALT_DOC>` |
| `rewrite-to-not-found` | the delete `--rewrite-to` target does not resolve to any document |
| `rewrite-to-self` | the delete `--rewrite-to` target resolves to the document being deleted |
| `rewrite-to-ambiguous` | the delete `--rewrite-to` target resolves to more than one document |

### Terminal — `new` target-resolution / plan-synthesis refusals

`norn new` / `vault.new`'s three-mode target resolution (NRN-230) and plan
synthesis. `assignment-malformed`, `field-json-invalid`, and
`field-type-invalid` are NOT re-documented here — they are `set`'s existing
codes above, reused because the semantic is identical (a malformed
`--field`/`--field-json` pair, an invalid `--field-json` JSON payload, and a
value failing schema-aware coercion, respectively).

| Code | Cause |
|---|---|
| `path-and-rule-conflict` | both a path and `--as` were supplied |
| `unknown-rule` | `--as RULE` names a rule absent from `validate.rules` |
| `rule-not-creatable` | the named rule has no `target` template |
| `missing-var` | a rule `target` references `{{var.NAME}}` / `{{path.NAME}}` not supplied via `--var` |
| `missing-title` | a rule `target` (or the inbox fallback) references `{{title}}` and no `--title` was given |
| `template-render-failed` | a rule template failed to render — either its path `target` or its `body` scaffold references an unknown placeholder, or is otherwise malformed (one code, both sites; the `message` names the failing site) |
| `seq-misplaced` | a rule `target`'s `{{seq}}` appears outside the file name, or more than once |
| `no-inbox-configured` | neither a path nor `--as` was given, and no `inbox.path` is configured |
| `inbox-requires-title` | the inbox fallback (no path, no `--as`) was used without `--title` |
| `path-ignored` | the resolved path is excluded by `files.ignore` (norn does not manage ignored paths) |
| `substitution-failed` | a `frontmatter_defaults` template value references an unresolvable substitution |

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
- [apply](commands/apply.md) — apply a MigrationPlan with precondition checks.
