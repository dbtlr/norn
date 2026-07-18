---
title: "0011 — Routed mutations commit at send; post-send never falls back"
description: "Architectural decision fixing the send-commit fallback policy for routed mutations: the tools/call frame write is the send boundary, post-send failures never fall back to direct execution (post-send-uncertain, exit 1, vs clean refusal exit 2), no CAS gate at send time, plans cross the wire as plan bytes."
---

# 0011 — Routed mutations commit at send; post-send never falls back

**Decision:** When a norn mutation routes to the warm daemon, the **send boundary is the write of the `tools/call` frame**. Every failure before that write (socket trust, connect, hello/ready, version gate, MCP initialize) is *pre-send* and falls back to direct execution silently. Every failure at or after the write is *post-send* and — in apply mode — **never falls back**: the CLI surfaces a typed `post-send-uncertain` error (exit 1, "the daemon may have applied this change") instead of re-running the mutation directly. Dry-run and preview modes route under a fallback policy (any failure → direct re-run), because a forecast is idempotent. There is **no compare-and-swap gate at send time**; the policy seams dispatch by exhaustive `match`, so the future CAS variant must be handled explicitly and against this record. Plans cross the wire **as plan bytes** (`vault.apply` ships the serialized plan, not a re-derivation), so what was reviewed is what is applied.

## Context

Phase 1.5 moved CLI writes onto the daemon path: `set`/`edit` (PR #113) and `move`/`delete`/`rewrite-wikilink` route through the `route_call` seam when a daemon is live, byte-identical to direct execution. Routing a *mutation* changes the failure calculus that reads never had: a read that fails mid-flight can always be re-run directly, but a mutation that *may* have executed on the daemon must not be retried — a fallback after an uncertain send is a **double-apply**. The adversarial review of the seam (#109) caught exactly this before any caller existed: a reconstruct-failure-after-successful-call branch that silently fell back under apply.

## The load-bearing choices

- **The frame write itself is classified post-send.** Today's newline framing means a failed write provably never reached the tool — but the classification deliberately does not exploit that. Mutation safety must not couple to the daemon's framing invariant; a future transport change (batched frames, TLS, retry-capable streams) must not silently convert "uncertain" into "safe to retry". A future reader will be tempted to "fix" this pessimism — do not, without replacing the guarantee it encodes.
- **Uncertainty is an operator-facing outcome, not an internal state.** `post-send-uncertain` (exit 1) is deliberately distinct from a clean pre-write refusal (exit 2). Exit 2 means "nothing happened, the refusal is authoritative"; exit 1 means "verify vault state before retrying" (`norn get <target>`, or re-run with `--dry-run`). Automation may branch on these codes; they are contract.
- **Clean daemon-side refusals must arrive as coded, report-shaped tool results** — never as bare protocol errors — precisely so they reconstruct as exit-2 refusals instead of drowning in the uncertainty bucket (the refusal-envelope parity work, including `mutation-lock-timeout`). Any new mutation tool must follow this shape or its refusals will surface as exit 1.
- **No CAS at send time.** Considered and rejected for this phase: the mutation lock spans preflight through apply on both surfaces, so the read-plan-write window CAS would guard is already excluded on a single host. CAS buys protection for a *narrowed* lock and cross-writer races, and lands as its own explicitly-matched policy variant later — retrofitting it silently here would have widened this ADR's guarantee without review.
- **Interactive TTY flows stay direct.** The preview→prompt→apply conversation holds the mutation lock across the prompt; routing would split it into two wire calls with no lock continuity and no CAS to bridge them. Agents (the daily path) use `--yes`/`--dry-run`/JSON and route; humans at a TTY keep exact current semantics.

## Consequences

- A routed apply's worst case is a false "uncertain" (exit 1 when the daemon in fact did nothing). That is the accepted price; the alternative worst case (a double-apply) corrupts a vault.
- `--config` / `--no-cache-refresh` force direct execution for mutations exactly as for reads — the wire speaks canonical vault roots only (pinned by the `forced_direct_flags_never_route` suites after a confirmed near-miss in review).
- CAS and lock narrowing must be designed against this record: the exhaustive-match seams will force *a* decision at compile time, but this ADR states what the correct handling must preserve.
## Amendment — 2026-07-17: fallback-to-direct and TTY-never-routes are superseded

Under [0017](./0017-registered-vaults-summoned-owners.md) there is no direct execution path to fall back to. The send-commit boundary itself carries forward unchanged — the write of the tools/call frame remains the point of no return, and post-send-uncertain (exit 1) vs pre-send refusal (exit 2) keeps its meaning. What changes: a pre-send failure now resolves by summoning a fresh owner and retrying the connection (bounded), then failing loudly — never by executing in-process. TTY flows route like every other invocation; the interactive confirm/preview happens client-side before send, so the safety model is unchanged while the execution path is unified. The mutation-lock-spanning-preflight-through-apply rationale for "no CAS at send time" carries forward inside the owner.
