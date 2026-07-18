---
title: "0015 — Plan preconditions assert exact owner sets"
description: "Architectural decision establishing exact owner-set assertions as MigrationPlan v2 preconditions, evaluated under the mutation lock before all writes and reported separately in ApplyReport v3."
---

# 0015 — Plan preconditions assert exact owner sets

**Decision:** MigrationPlan v2 carries first-class owner-set preconditions, evaluated as one barrier under the mutation lock before any operation writes. A precondition compares an exact, sorted set of vault-relative owner paths selected by a canonical stem, conjunctive field equality, or the resolved stem of a named create operation. ApplyReport v3 reports precondition outcomes separately from operations; a mismatch is a coded `owner-set-mismatch` refusal with expected and actual paths, while every operation remains not-run.

## Why

Logical identity can diverge from a physical mutation path between planning and apply. Path CAS alone cannot prove that a stem still has one owner, and project identity may instead be a field conjunction such as `type=project` plus `key=MMR`. Exact owner sets express both absence for create and sole ownership for mutation without baking a consumer domain into norn.

All create targets, including `{{seq}}`, are resolved before the barrier. Conflicting claims within the same plan refuse before writes. Dry-run forecasts the same barrier, while confirmed apply re-evaluates it under the lock.

## Rejected alternatives

- Reservations: add leases, crash cleanup, and durable lifecycle state for a pre-write assertion.
- Logical-target mutation operations: conflate target resolution with mutation and do not cover field-defined identities.
- Assertion pseudo-operations: make non-mutating safety checks look like writes and distort operation reports.
- Full query grammar: couples a safety contract to evolving search features; exact conjunction is sufficient.
