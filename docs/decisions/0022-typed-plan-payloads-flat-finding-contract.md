---
title: "0022 — typed plan op payloads and the flat finding contract"
description: "Architectural decision that migration-plan op payloads are typed structs in norn-wire decoded strictly (refusal over coercion), finding linkage rides as optional typed op fields echoed on the apply report, and validation findings cross every surface as one flat struct — no untagged enum, no leaked internal models, no pre-serialized string tunnel."
---

# 0022 — typed plan op payloads and the flat finding contract

The wire contract crate owns the full shape of what crosses it. **Decision:**
migration-plan op payloads (`change` and `edit` included) are real typed structs
defined in `norn-wire` and decoded exactly once, strictly — a wrong-typed field
is a refusal with a precise error, never a silent coercion or default. Finding
linkage (`finding_code` / `repair_rule`) is a pair of optional typed op fields,
echoed verbatim on the corresponding apply-report op and ignored by the applier.
Validation findings cross every surface — CLI JSON, the daemon wire, MCP — as
one flat struct:

```json
{ path, code, severity, message,
  rule?, field?, target?, candidates?: [path], next_actions?: [string] }
```

No untagged enum, no variant-specific field sets, no internal model embedded in
output. The pre-serialized-string carriage of findings on the wire
(`ValidateReport.findings: Vec<String>`) is retired in the same stroke: the wire
carries the typed struct.

The rule exists because every loosely-typed crossing point was a live defect
class: an op payload carried as a raw JSON map let wrong-typed values decode to
silent defaults (`"force": "true"` becoming `false`), let sibling decode arms
disagree on refusal ordering, and left the payload shape enforceable only at
runtime inside a downstream crate. The untagged finding enum leaked the internal
link-resolution model wholesale into user-facing JSON, making the output
contract an accident of internal representation. A consumer deciding fix-vs-skip
needs a stable, designed vocabulary — not a projection of whatever the engine's
internals happen to hold.

## Context

The plan/report path was progressively typed (ADR 0015, ADR 0016's consumer
coupling note): frames and the plan envelope became typed structs, but the
`change`/`edit` op payloads remained `Map<String, Value>` because their shape
was owned by an interior crate the wire crate cannot name. Findings were
serialized to JSON strings at the producer and tunneled through the wire so the
client could pass them through byte-identically — a shape frozen by internals,
not by design.

## The invariants

- **One decode, in the wire crate.** Op payload shapes live in `norn-wire`;
  interior crates consume the typed structs. The serialized plan keeps the
  pinned `kind` + `fields` JSON shape — the typing changes the decode, not the
  document.
- **Strict decode.** A present-but-wrong-typed field refuses with a precise,
  coded error. Absence is the only way to get a default. A `kind` that
  disagrees with an embedded `operation` discriminator refuses; a non-object
  `fields` refuses identically on every op arm.
- **Linkage is carried, never interpreted.** `finding_code` / `repair_rule`
  ride on ops as optional fields for provenance, echo onto the apply report's
  per-op record, and never influence apply behavior. Plans stay hermetic.
- **Findings are a designed contract.** One flat struct with a closed field
  set; `candidates` are plain vault paths, `next_actions` plain strings.
  Internal models (link resolution state, spans, parse context) never
  serialize into it.
- **No string tunnels.** A typed surface never carries a pre-serialized JSON
  string of itself; renderers serialize the typed value at the edge.

## Consequences

- `norn validate --format json` / `jsonl` per-finding output changes shape:
  variant-specific fields collapse into the flat contract and the leaked link
  model is gone. This is a breaking change taken in the pre-1.0 window with no
  compatibility shim; parity cases that pinned the old shape diverge from the
  pinned oracle by design and are ledgered against this decision.
- Malformed authored plans that previously decoded via silent coercion now
  refuse. Divergences from the pinned oracle on such inputs are deliberate and
  ledgered against this decision.
- The MCP validate tool can expose a schema for findings instead of opaque
  values, because the wire type is nameable and closed.
- Repair's plan-time skip detail (skip reasons, candidates, next actions)
  remains on the planner report, sibling to the plan — the flat finding
  contract is the read-side vocabulary, not a carrier for planner internals.

## Amendment (2026-07-22): coded refusals across the apply envelope

NRN-436 / NRN-236 extend this decision's "malformed plans refuse with a real
code" stance across the whole apply envelope, and harden the mechanism that
enforces it.

- **User-fault families carry precise typed codes.** Several authored-plan
  refusals raised while executing a `MigrationPlan` — the `create_document`
  guards (destination already exists, parent directory missing, resolved path
  ignored, non-object frontmatter, serialize failure), the owner-set
  precondition-validation faults (empty/invalid selector, duplicate id, missing
  named op, unparseable `eq` predicate), and the plan-structure faults
  (duplicate operation id, create path with no stem, missing/undecodable edit
  payload) — were bare `anyhow` errors invisible to the envelope's downcast
  ladder, so they flattened to `code: internal-error`. Each family now carries a
  typed error and a precise code: `create-destination-exists` /
  `create-parent-missing` / `create-ignored-path` (echoing `path`),
  `malformed-plan` (non-object frontmatter, serialize, plan-structure), and
  `invalid-precondition` (owner-set validation). Exit codes and messages are
  unchanged.
- **`internal-error` is reserved for genuine norn defects.** It is the fallback
  for an unrecognized error only. A consumer branching on `code` can now
  distinguish "your plan is malformed, fix it" from "norn crashed, file a bug".
- **Families register once, enforced by a guard test.** Recognizing a family
  used to mean editing three lockstep seams (the variant, its `code()` arm, and
  a bespoke `from_anyhow` downcast arm) with nothing checking all three landed —
  which is how these families were missed. A `CodedError` trait plus a single
  `REGISTRY` list collapse the third seam to one line, and a guard test
  constructs one instance of every registered family and asserts none maps to
  `internal-error`, so a half-landed family cannot ship.

Divergences from the pinned 0.48.1 oracle (which still reports `internal-error`
for these families) are deliberate and ledgered against this decision (PD-125).
