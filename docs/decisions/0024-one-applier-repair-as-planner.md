---
title: "0024 — one applier, repair as a planner"
description: "Architectural decision that a repair plan IS a migration plan: a single apply orchestrator consumes MigrationPlan on the typed-op vocabulary with per-file atomicity and true per-op status tracking, repair collapses to a pure planner emitting a suggested MigrationPlan, cascades stay derived-at-apply from a declarative op meaning, partial apply is the semantics (independent files proceed), and destructive/structural ops carry a plan-time hash."
---

# 0024 — one applier, repair as a planner

There is one apply orchestrator, and a repair plan is not a different kind of
thing it applies. **Decision:** a `MigrationPlan` of typed operations is the
single apply input; one orchestrator executes it with per-file atomic commits,
routing every op through the same low-level primitives the mutation verbs use,
and assembles a per-op report by tracking each op's outcome as it happens.
Repair stops being an applier: it is a pure planner that turns findings into a
suggested `MigrationPlan` (with finding linkage on each op) plus a sibling
plan-time report. The former repair-only apply path — `RepairPlan`,
`RepairApplyReport`, and the interior `PlannedChange` working type as a plan
contract — is deleted. Cascades (a move/delete keeping backlinks consistent)
stay **derived at apply time** from a declaratively-reframed op, shown as a
conditional forecast on dry-run and represented by skip/fail classes on the
report. Partial apply is the semantics: a per-file transaction that fails aborts
only its own file; independent files proceed; a plan-level barrier still refuses
everything before any write.

The rule exists because two apply paths were maintained for one job. `repair
apply` drove `apply_repair_plan_with_context` over a `RepairPlan`; every other
mutation verb built a `MigrationPlan`, and the executor **synthesized a
`RepairPlan` from it** to reuse that same orchestrator — a round-trip
(`MigrationOp` → `PlannedChange` → synthetic `RepairPlan`) whose only purpose was
to feed the one real applier through a repair-shaped hole. The report was then
reconstructed backwards from the telemetry event log because the orchestrator
returned a whole-plan `Ok`/`Err` with no per-op result. Two plan types, two
report types, and an interior model masquerading as a plan contract is three
representations of one migration; each is a place the applied semantics can drift
from the reviewed plan.

## Context

The plan/report path was progressively typed (ADR 0015 preconditions, ADR 0016
surface-neutral core, ADR 0022 typed op payloads and the flat finding contract).
The op vocabulary became typed structs decoded once in `norn-wire`
(`TypedOp::try_from`), and finding linkage became optional typed op fields echoed
on the report. But the *applier* still consumed the interior `PlannedChange` and
the repair-only `RepairPlan`/`RepairApplyReport`, so the typed op vocabulary was
adapted back into the old models at the execution boundary. This ADR finishes
0022's arc: the typed op vocabulary is what the applier consumes, and the
migration plan is the only plan.

Two facts made the collapse safe to state now. First, `MigrationPlan` already
carries everything a repair plan needed — typed ops, per-op `footnote`, per-op
finding linkage (ADR 0022), and a `skipped` list — so repair emits one natively
without inventing carriage. Second, the repair-time rich skip detail (skip
reason, candidate paths, next actions) is a *planner* output, not an *apply*
output; ADR 0022 already sited it "on the planner report, sibling to the plan,"
so moving repair to a pure planner puts that detail where the decision already
said it belonged.

## The invariants

- **One plan, one applier.** `MigrationPlan` is the only apply input.
  `apply_migration_plan` is the only orchestrator. `RepairPlan`,
  `RepairApplyReport`, and `PlannedChange`-as-plan-contract are deleted; there is
  no synthetic-plan round-trip. The applier consumes the typed-op vocabulary and
  routes each op through the same primitives the verbs use.
- **Repair is a pure planner.** `plan_repairs` emits a `MigrationPlan` directly
  — typed ops carrying finding linkage and, for destructive/structural ops, a
  plan-time `document_hash`. `repair --plan` writes that plan; `repair apply`
  feeds it to the one applier with zero repair-specific apply code. Findings →
  plan is planning; plan → vault is applying; the two never re-entangle.
- **Linkage is carried, never interpreted.** Each op's `finding_code` /
  `repair_rule` (ADR 0022) ride the plan as provenance and echo onto the report's
  per-op record. The applier never reads them to decide behavior.
- **Rich skip detail lives on the planner report.** A finding repair chose not to
  act on carries its skip reason, candidate paths (as plain strings), and next
  actions on repair's plan-time report — a sibling to the plan, additive fields
  omitted when empty, mirroring the flat finding vocabulary (ADR 0022). The
  applier's report never carries planner internals.
- **Cascades are derived, declaratively framed.** A reviewed `move_document` /
  `delete_document` op means "relocate/remove this document *and keep incoming
  links consistent*"; the backlink rewrites are not enumerated in the plan. The
  applier derives them at apply time from the settled content state. A dry-run
  forecast shows the derivation marked conditional; the report represents its
  outcome as applied / skipped / failed cascade classes.
- **True per-op tracking.** The orchestrator records each op's status
  (`applied` / `failed` / `not_run` / `skipped`) as the op runs, not by
  reconstructing it from the event log after the fact. The report's tallies —
  including `remaining` — are the direct sum of those recorded statuses.
- **Partial apply is the semantics.** Each file is one atomic transaction
  (fingerprint → shadow → verify → swap). A transaction that fails aborts only
  its own file; ops on independent files still run. A failure after any write has
  landed is a truthful partial-failure report (`outcome = failed`, exit 1), never
  a byte-identical refusal. A plan-level barrier (schema gate, containment,
  duplicate-id, `requires` validation, owner-set preconditions) refuses the whole
  plan before the first write; that alone is the byte-identical `refused` (exit 2)
  class.
- **Ordering is a constrained DAG.** An op may declare `requires` referencing
  another op's `id`. An unknown reference or a cycle is a `malformed-plan`
  refusal. `requires` constrains ordering *within* the existing kind-ordered
  passes (content → delete → create → move → link-cascade → retry); a plan that
  declares no `requires` orders exactly as before.
- **Destructive/structural ops carry a plan-time hash.** `move_document` and
  `delete_document` fields gain an optional `document_hash`, stamped at plan
  synthesis time from the loaded index (the pattern repair's planner already
  used), not hydrated from the live index at apply time. A present hash is a
  compare-and-swap precondition checked via the transaction fingerprint path
  (delete) or pre-rename (move); an absent hash means no check. Delete-hash
  *required* remains an open question, deliberately left un-forced here.

## Consequences

- The `MigrationOp` → `PlannedChange` → synthetic-`RepairPlan` round-trip and the
  event-log report reconstruction are both gone. The report is assembled from
  recorded per-op outcomes, so a new op kind cannot silently report the wrong
  status because its telemetry event name was unmapped.
- Verb-synthesized `move` / `delete` plans change bytes: their op `fields` gain
  `document_hash`. This shifts `MigrationPlan::canonical_hash()` and the plan JSON
  for those verbs; plan-shape parity cases diverge deliberately, ledgered against
  this decision.
- Independent-file progress is observable: a multi-file plan where one file's
  transaction fails now applies the others and reports the failure per-op,
  where the single abort-the-plan path previously left more ops `not_run`.
  This is a deliberate parity divergence class, ledgered against this decision.
- Repair's rich skip detail moves onto the wire repair report as additive fields;
  a consumer reading skip candidates/next-actions reads them there, not off a
  `RepairPlan`. Additive and omitted-when-empty, ledgered against this decision.
- The executor no longer rebuilds refused-vs-failed from a runtime `wrote_any`
  flag plus telemetry: per-op status makes the outcome a direct tally, and the
  write-state gate is retained only to distinguish a clean refusal from a partial
  failure at the transaction boundary.

## Relation to prior decisions

- **Extends ADR 0022.** Takes 0022's typed op vocabulary and finding-linkage
  fields all the way into the applier, and honors 0022's siting of repair skip
  detail "on the planner report, sibling to the plan."
- **Extends ADR 0016.** The surface-neutral core now has one plan model and one
  applier for every mutation verb and for repair.
- **Under ADR 0018.** Lands on the `rewrite/0017` line; the divergences named
  above are recorded in the oracle-parity ledger citing this ADR.
