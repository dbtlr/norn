---
title: Architecture
description: The standing architecture invariants — the properties every change preserves — distilled from the decision records into a single checklist a pull request can be tested against.
---

# Architecture

This page records `norn`'s **standing architecture invariants**: the properties every change is expected to preserve. It is the companion to the [decision records](./decisions/) — an ADR captures one decision at the moment it was made, with its context and its trade; this page distills the invariants those decisions leave behind into a checklist a change can be tested against. Where a principle rests on a specific decision, it cites the ADR by number.

Read it before adding a surface, a crate edge, or a mutation path. The point is to reach for the seam that already exists instead of re-deriving the base system: most new work has a home here, and the fastest correct implementation is usually the one that respects these lines. A reviewer can apply each principle as a concrete test on the diff.

## 1. Contract types live in norn-wire

**Every end-user shape — a report, a plan, a warning, an error envelope — is a typed struct in `norn-wire`.** `norn-core` constructs the values and the client surfaces render them; nothing else defines the shape. No `serde_json::Value` and no JSON-embedded-in-a-`String` may cross the owner wire without a dated ledger ruling that says why. `norn-wire` is pure serde types with no logic and no I/O, which is what keeps the client-side crates off `norn-core` — they depend on the vocabulary, not the engine. A crate dependency edge must carry real code: a declared edge with no call behind it is a manifest lie and comes out (ADR [0016](./decisions/0016-surface-neutral-command-core.md), [0018](./decisions/0018-greenfield-rewrite-oracle-parity.md)).

## 2. Typed facts cross boundaries; prose is rendering

**A boundary carries facts — severity and code enums — never a rendered sentence.** Each client surface (CLI, MCP) composes its own message fidelity from the same typed facts, so two surfaces need agree only on meaning, never on wording. No outcome bit may be derived from message text: the code decides the outcome, the text only explains it. The stderr prefix vocabulary is a closed set defined once in the display layer, and `format!("{:?}")` never appears in display code (ADR [0020](./decisions/0020-posix-by-default.md), [0021](./decisions/0021-verbs-return-one-layer-renders.md)).

## 3. One user-facing vocabulary per cross-verb concept

**A value that appears in more than one verb has exactly one spelling.** A verb's flag enum may subset the shared value set — expose fewer choices — but it may never rename a value or invent a synonym for one. One shared parser owns the names, so a value means the same thing everywhere it is accepted, and `--help` cannot advertise a variant the parser would reject (ADR [0010](./decisions/0010-cli-grammar-forgiving-inputs.md), [0021](./decisions/0021-verbs-return-one-layer-renders.md)).

## 4. One render seam, per-verb units

**Verbs return data; a single display layer renders it.** Each verb's renderer is a single-concern module sitting behind one `emit` entry point, and that entry point resolves the palette and the output format exactly once. A verb never writes stdout, never resolves color, and never decides tty-versus-pipe defaulting — the payload stream is clean by construction because only the layer can reach it (ADR [0021](./decisions/0021-verbs-return-one-layer-renders.md)).

## 5. Owner startup failures classify as user-error or heal

**A failing owner startup is either the caller's mistake or the owner's fault, and the two are handled differently.** A user mistake — a bad config, a missing vault root — is answered as a `Rejected` frame carrying the real cause and then eager-reaped, so the client sees the reason and the dead owner does not linger. An internal fault, such as a cache that will not build, exits-to-heal, letting the next invocation summon a fresh owner. A failure cause that reaches only a log file is a defect: the caller must be told why (ADR [0017](./decisions/0017-registered-vaults-summoned-owners.md)).

## 6. Unpinned surfaces are still contract

**Behavior that no parity case pins is still behavior norn owes.** The oracle harness pins much of the surface, but a gap in coverage is not a license to drift. A divergence discovered on an unpinned surface earns a parity case and a [ledger](./parity-ledger.toml) entry recording the intended old-to-new difference, landing with — or before — the change that introduces it, never silently (ADR [0009](./decisions/0009-cli-mcp-surface-parity.md), [0018](./decisions/0018-greenfield-rewrite-oracle-parity.md)).

## 7. An extraction completes in one PR

**Moving an engine down into a lower crate is one atomic change, not a migration.** The PR that relocates code switches every consumer to the new home and deletes the source copy in the same change — there is no interval where two copies coexist. A public API with no workspace consumer fails CI, so a half-finished extraction cannot land looking done (ADR [0018](./decisions/0018-greenfield-rewrite-oracle-parity.md)).

## 8. The cache is an incremental index

**The cache is a derived index that is maintained, not recomputed.** A mutation writes through its own known post-state: it composed the files, so it re-derives nothing. A targeted refresh of the touched paths is the fallback where write-through does not apply, and full derivation runs only at cold start, on integrity failure, or in the scheduled off-request health scan — which shards at scale so auditing never blocks serving. Request paths never pay derivation costs (ADR [0005](./decisions/0005-trusted-cache-via-warm-service.md), [0013](./decisions/0013-generational-contexts-two-class-writer.md), [0014](./decisions/0014-atomic-cache-publication.md)).

## 9. Determinism includes artifacts

**Identical inputs produce identical bytes, artifacts included.** A plan or report hash covers semantic content only — wall-clock time never feeds a hash, so two runs of one operation are byte-comparable. Paths normalize at the resolution boundary, so the exact spelling of an argv path never reaches a durable artifact. This is what lets the parity harness be a mechanical byte comparison rather than a fuzzy one (ADR [0018](./decisions/0018-greenfield-rewrite-oracle-parity.md)).

## 10. One plan, many planners, one applier

**`MigrationPlan` is the only plan vocabulary, and one applier executes it.** Repair — and any future source of change — is a *planner*: it composes a `MigrationPlan`, it does not apply one. A single orchestrator (the plan-load + schema-gate + expansion + report-assembly executor driving the ordered named passes) applies every plan with per-file atomicity, routing through the same filesystem primitives the direct mutation verbs use; a per-op failure records against that op while independent ops still run, and only a plan-level barrier refuses the whole plan pre-write. There is never a second mutation engine (ADR [0015](./decisions/0015-plan-preconditions-owner-sets.md), [0024](./decisions/0024-one-applier-repair-as-planner.md)).

## 11. The substrate bears the trust burden

**Trust is maintained continuously by the substrate so that requests can stay fast.** A warm owner, a filesystem watcher, and a periodic health scan keep the served state trustworthy between requests; a request presumes that work is done and answers from it. Checks run on signal — an obvious integrity failure acts immediately and conservatively — or on schedule, never as a per-request ritual. Recovery from an eviction is background work owned by the maintenance layer, not a cost billed to the next caller (ADR [0005](./decisions/0005-trusted-cache-via-warm-service.md), [0013](./decisions/0013-generational-contexts-two-class-writer.md), [0017](./decisions/0017-registered-vaults-summoned-owners.md)).

## See also

- [Concepts](concepts.md) — the vault graph, frontmatter, and validate/repair loop these invariants operate on.
- [Development](development.md) — build, test, and the per-task verification gate.
- [Decision records](./decisions/) — the dated decisions these invariants distill.
</content>
</invoke>
