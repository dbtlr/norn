---
title: "0017 — Registered vaults, summoned owners: always-routed access, disposable derivation, owner-held trust"
description: "ADR: vaults are addressed by registered name; every invocation routes to a summoned daemon owner; the cache db is disposable derivation; trust moves from per-request proof to owner machinery (watch + drift audit). Reverses 0016's cold-path rejection and amends 0005's optional-accelerator posture."
---

# 0017 — Registered vaults, summoned owners: always-routed access, disposable derivation, owner-held trust

**Decision:** norn addresses vaults **by registered name** through a bidirectional registry (name → root, root → name; CWD resolution is sugar). **Every invocation routes to a daemon** — the CLI never opens a cache in-process; a daemon is *summoned* on demand (ephemeral tier for unregistered vaults and all dev builds; resident tier for registered vaults; managed tier for supervised installs/HTTP MCP) and reaps itself after an idle TTL. **Registration gates all durable artifacts** (persistent cache entry, event stream, logs); unregistered work is tmp-homed and disposable. The **SQLite cache db is pure derivation** — created at summon, deleted at owner exit, so client-vs-cache schema mismatch is unrepresentable. **Trust is maintained, not proven**: a file watcher (fast path) plus a drift auditor (truth loop, health metrics) carry the burden of proof between requests; the request path presumes a warm vault is valid. Design of record: *the registered-vault architecture note (internal design doc)*.

## Context

The 2026-07-17 evidence chain (a +461-entries-per-test-run leak, a day-long race chain, a 61k-entry / 8 GB consumer leak) traced every active bug class to one premise: the cache as a global, ambient, shared-mutable tree written by uncoordinated processes, each entry's lifecycle bound to nothing. The founding "safety > speed" bias — re-prove freshness on every request — was correct pre-daemon (every invocation cold, nothing guarding files between calls) and wrong after: it makes the daemon an over-engineered passthrough, performant to date only because vaults were small. The placement itself (`~/.cache/<hash>`) was an unshaped artifact of founding commit `be02575`, not a decision.

## Load-bearing choices

- **Identity is assigned, never derived.** The registry assigns vault identity; nothing infers it from CWD, canonical paths, or environment. (the "identity travels with the artifact" principle applied at vault altitude — and required by the HTTP MCP roadmap, where off-system consumers cannot know directories.)
- **Registration is explicit, never implicit.** Consumers call it programmatically as a stable setup artifact; implicit registration would recreate the leak class inside the registry.
- **One owner per entry, enforced by the existing lifetime flock** (ADR 0005). Dev builds run the ephemeral tier unconditionally (ADR 0012's fingerprint keys the socket), so a dev binary structurally cannot touch registered entries.
- **The invariant is relocated, not abandoned.** Verification moves off the request path into owner machinery; the drift auditor is the "don't skip" half of trust-over-speed. Observed drift ≈ 0 is what licenses presumption; observed drift > 0 is a watcher bug surfacing as a metric, not as wrong results.
- **Warm-up-on-summon is the accepted cost** (~linear in vault size; ~500ms/1.2k docs today). Mitigations: progress surface on self-update and warming queries; managed tier for those who never want cold starts. A future opt-in non-ephemeral db model is the named escape hatch, deliberately deferred.

## Reversals and amendments (owned explicitly)

- **Reverses ADR 0016's rejection of "always-route-to-daemon (kill the cold path)."** What changed: 0016 evaluated routing as an *accelerator* while trust still lived in per-request proofs, so a cold path had to stay budget-compliant. Once trust moves into the owner, a non-routed path is not a cheaper equivalent — it is an untrusted parallel implementation. 0016's seam (adapter → dispatch → execute) survives intact; its dispatch answers "summoned or resident," never "routed or direct."
- **Amends ADR 0005:** the daemon ceases to be an optional accelerator and becomes the sole access path; verify-once-hold-warm becomes verify-once-**watch-thereafter**. The lifetime flock and one-host-N-contexts topology carry forward unchanged.
- **Amends ADR 0006:** the registry gains its authoritative role (identity + durability gate), beyond client-side name resolution.
- 0001–0004, 0007–0015 are relitigated separately in the redefinitional grooming pass; supersessions land as explicit status changes, not silence.

## Consequences

- The leak, race, skew, and identity-war bug classes are prevented by construction; the prune/sweep/marker apparatus, multi-schema db matrix, channel-in-path machinery, and per-request freshness proofs are **deleted, not improved**.
- The daemon becomes load-bearing infrastructure: summoning must be robust headless, orphan owners must be detectable (flock + TTL), and crash blast radius is all-vaults-cold-until-resummoned (automatic).
- Registration becomes public consumer contract (a consumer adopts on its pinned-upgrade schedule per ADR 0016's elective-coupling doctrine).
- The task queue and workspace notes predating this decision are suspect until re-vetted; the redefinitional grooming pass is part of this decision's execution.
## Amendment — 2026-07-17: execution model — greenfield on a branch, oracle-anchored parity

[0018](./0018-greenfield-rewrite-oracle-parity.md) reshapes *how* this decision is built: greenfield-in-place on the long-lived `rewrite/0017` branch (old `src/` retired as documentation-not-source, ported organ-by-organ until deleted), interfaces ported first as executable 1:1 constraints under an oracle parity harness, new-world surfaces built after parity. The *what* of this ADR is unchanged. The phased plan in *the registered-vault architecture note (internal design doc)* is superseded by the phasing in *the rewrite-execution-model design doc (internal design doc)*; the crate map gains `norn-wire` and `norn-frontmatter` per the amended *target-architecture-boundaries design doc (internal design doc)*. The ADR corpus and glossary move into the repo (`docs/decisions/`) as the authoritative home.
