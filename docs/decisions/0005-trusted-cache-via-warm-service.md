---
title: "0005 — Cache integrity is relocated to a warm service, never weakened"
description: "Architectural decision relocating norn's SQLite cache integrity verification from a per-invocation CLI check to a persistent warm norn-service process, with the CLI falling back to direct verification when no service is available."
---

# 0005 — Cache integrity is relocated to a warm service, never weakened

**Decision:** norn resolves the per-invocation `PRAGMA integrity_check` cost by **relocating** verification to a persistent, single-owner process — **`norn-service`** — that opens the vault's SQLite cache once, integrity-verifies it, and holds it warm for its lifetime. `norn-cli` gains a routing seam: it cheaply probes for a live service and, if one answers a handshake promptly, translates its arguments to the existing MCP tool contract and **delegates the entire data operation**, rendering only the response. If no service is present or the probe times out (a hung service), the CLI **falls back to a direct, integrity-verified open** — today's behavior. The integrity check is **never throttled, downgraded (`quick_check`), or gated behind a staleness window.** Trust is **inherited from a live verifier or re-established locally, never skipped.**

The service is not a new component: it is the existing `norn mcp` server (already an `rmcp`-based `ServerHandler` holding a warm `VaultContext`) upgraded from a stdio one-shot into a persistent listener, plus an open-once/verify-once warm cache. Design-of-record: *the norn-service architecture design doc (internal design doc)*.

## Context

`Cache::open`'s `inspect_existing_cache` runs `PRAGMA integrity_check` on every process open (`src/cache/open.rs`). The check is O(db pages); at ~1k docs (a real-world vault) it is ~0.2ms and invisible, but at 50k docs it dominates sub-millisecond routed queries — the timing harness had to measure in-process to isolate the router from it. Wave 2's EAV index made queries flat with scale, so per-invocation open cost is now the dominant large-vault cost.

The tempting fix — throttle the check, use `quick_check`, or stamp a "last verified" lease and trust it for a window — was **rejected on principle, not on cost.** For norn, trust is ~1000× more valuable than speed: reading the vault through norn must always feel like touching the actual files, so no path may serve unverified or lagged cache state (feedback: trust ≫ speed). The check is not over-engineering; it is the guarantee. The correct reframe is that the check is not too expensive but **in the wrong place** — it runs per-invocation only because the CLI is a cold one-shot with nowhere to amortize it. A process that lives long enough pays it once.

Two facts make the relocation cheap and the trust model clean:
- The MCP server *already* re-opens and re-verifies the cache **per tool call** (`src/mcp/mod.rs`), so a warm cache is an independently-justified latency win for every MCP call at every scale — the integrity-check cost becomes a free consequence.
- `rmcp` 1.7.0 serves over any `AsyncRead + AsyncWrite`; a `UnixStream` splits into exactly that, so the local daemon is a thin accept loop wrapping the existing `McpServer`.

**Alternatives rejected:**

- *Throttle / `quick_check` / time-lease* — all serve unverified or lagged state to save time; disqualified by the trust invariant.
- *`--cache-only` flag that skips the in-process check* — the one move that trades trust for speed with no live verifier backing it. Subsumed: the trust-preserving way to "go cache-only" is to route to the live service, never to skip the check.
- *Time-stamped verification lease in `meta`* — a stale-lease window must be reasoned about and can be served after a watcher hangs. Replaced by **per-request liveness**: the CLI trusts a *live* handshake answer for *this* request, so a dead or hung service simply cannot answer and the CLI self-verifies. No window exists.
- *A filesystem watcher as the phase-1 mechanism* — the warm daemon alone kills the integrity-check cost (verify once, hold warm); the watcher is a later freshness optimization carrying the real risk (fs-event reliability, missed events, cross-platform), off the critical path.

## Consequences

- **The service owns data; the CLI owns presentation.** The service does all query/verify/write work on the warm cache; the CLI owns arg-translation in, result rendering out, and the human plan→confirm→apply step. This is the `norn-core` (synchronous library) / `norn-cli` / `norn-service` seam.
- **Single owner sidesteps concurrency reasoning.** A live service means exactly one process touches the cache; its absence means exactly one process (the CLI) does — never concurrent writers. (SQLite WAL would tolerate concurrent access; not having to reason about it is worth more on a trust-critical path.)
- **No core concurrency rewrite.** `norn-core` stays synchronous ("take a connection, do the work"); the service *orchestrates* concurrent use of it. The concurrency upgrade (`spawn_blocking` + read-connection pool + single serialized writer) is the code's own already-noted v2, not a rewrite.
- **The fallback path is load-bearing and permanent.** Routed and direct execution must remain semantically identical — the CLI↔MCP capability isomorphism is what makes the routing safe, and it must not drift.
- **New risk surfaces:** socket/daemon lifecycle (orphaned sockets, hung-detection accuracy), and per-request state leaking through process globals in a long-lived server (a scoped `norn-core` audit gates phase 1).
- **The remote transport collapses into "add a listener."** Streamable-HTTP / off-host vaults become phase 4 on the *same* `McpServer`, not a separate project — realizing the "MCP off-filesystem access as a consequence" stance of [0003](./0003-relational-engine-over-flat-files.md).
- **The integrity-check cost leaves the hygiene bucket** and becomes a consequence of the `norn-service` initiative.

## Phased delivery

Detailed sizing and de-risking live in *the norn-service architecture design doc (internal design doc)*. In brief: ① routing seam + warm local UDS daemon (reads first) — kills the integrity-check cost and warms every MCP call; ② concurrency upgrade; ③ filesystem watcher; ④ HTTP/remote transport (absorbs off-host vault access).
## Amendment — 2026-07-04: one host daemon, not N per-vault daemons

The topology is revised: **one host daemon with lazy per-vault warm contexts**, not one daemon per vault. `norn serve` listens at a single well-known socket (`<XDG_CACHE_HOME|~/.cache>/norn/run/norn.sock`); each connection's preamble names the canonical vault root, and the daemon derives the vault identity (`vault_identity()`) to open + integrity-verify that vault's cache once on first touch, holding the context warm thereafter.

Two findings forced the reversal. The derived per-vault socket path (`~/.cache/norn/<64-hex>/service.sock`) measured 101 chars against macOS's ~104-byte `sun_path` limit — the daemon's `bind()` fails outright for a longer HOME. And per-vault daemons multiply every operational surface (start, status, stop, upgrade-restart, supervision) by N, which in practice means no daemon ever runs.

The decision this ADR records is unchanged: verification is relocated to a warm process, never weakened. Verify-once-hold-warm is per *vault context*; a single lifetime flock makes the host daemon the sole routed owner of every cache it touches; requests serialize per vault. Routing remains derivation, not a registry — the vault identity travels in the connection preamble instead of the socket path, and the wire speaks canonical paths only (short-name resolution is a client-side phonebook concern — ADR 0006). Accepted costs: crash blast radius is all vaults cold at once (fallback is Direct), and daemon memory grows with vaults touched (idle-eviction later if it matters).
## Amendment — 2026-07-16: direct-path byte-integrity moves to rebuild-on-corruption

## Amendment — 2026-07-17: amended by ADR 0017 — daemon becomes the sole access path

[0017](./0017-registered-vaults-summoned-owners.md) ends the daemon's "optional accelerator" posture: every invocation routes to a summoned owner; the Direct verified-open fallback is deleted rather than maintained as a parallel trust implementation. Verify-once-hold-warm becomes verify-once-**watch-thereafter** (watcher fast path + drift auditor truth loop). The 2026-07-16 rebuild-on-corruption amendment is mooted with the direct path it patched. The lifetime flock (one owner per cache) and the one-host-lazy-contexts topology carry forward unchanged — ownership was this ADR's deepest pin, and 0017 universalizes it.

**Decision (shipped):** the direct/cold path no longer runs `PRAGMA integrity_check` on every open. Byte corruption (`SQLITE_CORRUPT`/`SQLITE_NOTADB`) is handled where it surfaces: an evict→rebuild→retry-once wrapper at the direct read helpers, and classification at the top-level CLI error seam for corruption first touched at query time — always under the shared cache write lock, always with an operator notice (`vault: cache is corrupted; discarding it — rebuilding from the vault`), always failing closed. The daemon keeps verify-once-per-generation; `cache status` and rebuilds keep the full scan.

**What changed and what did not.** The original rejection of throttle/`quick_check`/time-lease stands — those serve *lagged or unverified freshness state*, and the freshness machinery (probe, refresh, generation invalidation) is untouched by this amendment. What moved is the *byte-integrity* posture on the cold path: from prove-by-preflight-scan to re-establish-by-reconstruction. The cache is a rebuildable derived artifact of the vault; on detected corruption the response is discard-and-rebuild from source, which is a stronger recovery than a preflight scan (the scan only detects; reconstruction repairs). No wrong data is knowingly served: detection fails the current operation closed, and the rebuilt cache is constructed fresh from the vault files.

**Why now.** The preflight scan is O(db-size) and the db grew: measured 41ms on a 29MB / ~1.5k-doc dogfood vault (2026-07-16 architectural review) — ~34ms of every direct read, projecting to ~400ms at 10× scale, 8× the 50ms budget. The 2026-07-02 context measured ~0.2ms; the cost assumption the original per-open check rested on no longer holds. Post-change: find 47.8→15.8ms, count 54.0→22.4ms.

**Accepted risk.** SQLite does not checksum pages by default, so corruption that mimics valid structure can in principle return wrong data without erroring; the preflight scan shared this limitation (it detects malformed structures, not semantic bit-rot). The practical detection surface is unchanged; the detection point moved from open-time to first-touch.

Review record: PR #144 — two majors caught in review (query-time corruption escaping the retry boundary; unlocked eviction racing concurrent writers) fixed and re-verified before merge. In-process full-command retry was rejected because set/edit share the read helper and are non-idempotent; the chosen semantic is fail-closed-then-heal-next-invocation.
