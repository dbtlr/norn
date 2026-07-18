---
title: "0013 — Generational vault contexts over a two-class single-writer queue"
description: "Phase 2 concurrency model: immutable per-generation contexts bound at request boundary; one writer queue with liveness-over-bulk classes and file-coherent ~50ms chunks; WAL snapshot reads; control-plane busy/hung; watcher lands as a bulk writer. Amended 2026-07-17 (ADR 0017): the Direct fallback is deleted — no owner means summon."
---

# 0013 — Generational vault contexts over a two-class single-writer queue

> **Amended by [0014](./0014-atomic-cache-publication.md):** generation and queue choices remain accepted; visible per-file cache commits do not. Bulk chunks stage privately and publish one relational snapshot. **Amended by [0017](./0017-registered-vaults-summoned-owners.md) (2026-07-17):** the Direct fallback is deleted — no owner means summon; see the dated amendment at the end.

## Context

Phase 1 of norn-service serialized every daemon tool call through one per-vault async mutex (`call_lock`): correct, but any slow operation (a migration apply, a rebuild-on-change refresh) becomes every caller's latency, "busy" is indistinguishable from "hung" at the protocol level, and the Phase 3 filesystem watcher — a background writer inside the daemon — has no defined slot to write into. Scale arguments were explicitly rejected as the frame ("what does it fix, never mind scale"); the phase exists to remove those three defects, and it is designed with its known destinations in hand (watcher, client contract, bulk-write yield policy) rather than deferring them into later refactors.

## The load-bearing choices

- **Generational contexts.** A `VaultContext` generation is immutable once opened; every request binds `(generation, config)` by `Arc` at its request boundary and holds both to completion. Every evict/re-open path — cold start, ground-shift, cache-identity change, index-relevant config change — is a writer-queue op that opens generation N+1; in-flight requests drain on N, which drops with its last `Arc`. No request ever observes a swap. Opens become single-flight by construction, structurally retiring `call_lock`'s original cold-open-race job.
- **One writer, two classes, chunk-boundary preemption.** All write-shaped work funnels through one queue per vault: **liveness** (freshness refreshes, generation opens — work a reader is blocked on) preempts at the next chunk boundary; **bulk** (mutation chunks, watcher batches) runs FIFO. Bulk work commits in **file-coherent chunks** (a chunk never splits one document's rows) sized to yield roughly every 50ms. Bulk chunks prepare state privately; one terminal transaction publishes the complete relational cache snapshot (see [0014](./0014-atomic-cache-publication.md)). Chunk boundaries remain liveness-preemption seams, never visibility seams; logical mutation atomicity still stays with plan/apply and the per-vault mutation lock.
- **Reads are pooled and snapshot-served (WAL).** N read connections + 1 write connection per generation. A read runs concurrently iff its request-boundary freshness probe passes; a stale probe routes it through a coalesced liveness refresh. A read during the daemon's own in-flight write serves the pre-commit snapshot — verified-current until commit, so trust-over-speed is preserved without queueing readers behind writers.
- **Liveness from a vault-scoped control plane, not call timeouts.** `ping` additively carries an optional canonical `vault_root`; a fingerprint-compatible `pong` returns that vault's `serving` state (`cold | opening | ready`) and `writer_progress { busy, sequence }`. The opaque `u64` sequence is forward progress, not wall-clock time: it advances on open transitions, completed liveness work, bulk chunk boundaries, and terminal completion/drop/panic. Busy includes generation-open work. A live idle writer is healthy; only `busy` with a sequence unchanged beyond the service stall budget is hung. Healthy busy (`pong` alive, sequence advancing) waits indefinitely. No pong or a busy stalled writer originally sent reads and pre-send mutations Direct with the standard stderr note — superseded 2026-07-17 (see the amendment below): no pong now means summon an owner and connect, and a stalled writer is an owner-health event resolved by restarting the owner. Post-send mutations stay `post-send-uncertain` (unmoved). Non-writer tool-body progress is outside this contract. Plain `norn service status` remains host-level; `norn service status --vault <PATH>` opts into the vault state.
- **The watcher is just another bulk writer.** Phase 3 lands as coalescible bulk batches plus a second implementation of the freshness-probe interface — no new serving machinery.

## Consequences

- Fast calls stop paying for slow ones; busy/hung is decidable; the watcher has a slot before it exists. Sequencing settles as Phase 2 → Phase 3.
- Costs accepted: WAL grows for the duration of a bulk chunk (bounded by the yield knob); a pathological long-lived reader could pin a dead generation (all current calls are short; debug-asserted, not designed for).
- **Named non-goal:** any future streaming/subscription surface must be generation-aware from birth — its source of truth can be swapped under it mid-stream. Nothing else in this design accommodates streams.
- Fold-ins: post-apply index discard dissolves into "apply commits its own increments"; mutation-lock scope is forced by the chunk model.

## Amendment — 2026-07-17: the Direct fallback is deleted; no owner means summon

[0017](./0017-registered-vaults-summoned-owners.md) makes the summoned owner the sole access path, superseding this ADR's Direct escape hatches: **no pong now means summon an owner and connect** (ephemeral tier for unregistered vaults), never a direct in-process open; a busy writer stalled beyond the service stall budget is an owner-health event — surfaced, and resolved by restarting the owner — not a reroute. Post-send mutations stay `post-send-uncertain` (unmoved). The generational contexts, two-class writer queue, chunk-boundary preemption, snapshot reads, and control-plane liveness contract all carry forward unchanged inside the owner.
