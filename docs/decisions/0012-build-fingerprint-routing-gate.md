---
title: "0012 — Routing requires an exact build fingerprint; skew falls back Direct"
description: "Architectural decision gating daemon routing on a blake3 source-content build id carried in the handshake pong: version, then build, then protocol; any mismatch (including a missing field from an older daemon) falls back Direct silently, and service status reports same-version rebuilds as restart-pending."
---

# 0012 — Routing requires an exact build fingerprint; skew falls back Direct

**Decision:** A norn CLI routes to the warm daemon only when the daemon is the **same build**, not merely the same version. `build.rs` emits `NORN_BUILD_ID` — a blake3 fingerprint over the sorted `src/` tree (relative paths + contents) and `Cargo.lock` — and the handshake pong carries it as a serde-defaulted optional field. The gate checks **version → build → protocol** (extending the earlier handshake-gate ordering); any mismatch, including a pong missing the field (an older daemon), falls back to Direct execution silently with a one-line operator note. `norn service status` compares build ids the same way, so a same-version rebuild reports `restart pending (rebuilt)`.

## Context

The exact-version handshake gate covers released-binary skew completely — a v0.45 daemon can never serve a v0.46 CLI. But two **dev builds of one version** can speak different wire schemas: additive `ApplyReport` fields (cascade, `link_impact`) rendered as zeros when a stale same-version daemon served a newer CLI, observed live while dogfooding. Per-field policies do not generalize: a REQUIRED wire field forces a clean Direct fallback for reads (the `vault.repair` `has_diagnostic_errors` pattern), but a **committed mutation** whose report lacks a cosmetic field cannot fall back — failing reconstruct there would fabricate `post-send-uncertain` alarms over a rendering field. `CONTROL_PROTOCOL` is reserved for handshake-frame breaks and relies on a human remembering to bump it.

## The load-bearing choices

- **Source-content hash, not a build timestamp.** Identical sources produce identical ids: a no-change rebuild keeps routing (no gratuitous daemon orphaning), and all release binaries built from one source share one id — correct, since they speak one wire. Relative paths keep the id reproducible across checkouts and the release build matrix.
- **The pong field is optional on the wire, mandatory at the gate.** An old daemon's pong still parses (no protocol break); its missing field fails the gate into the same silent fallback. Deploys cleanly over any running daemon.
- **Version is still compared first.** A different released version reports as version skew (the more actionable message); build skew is the same-version refinement. Protocol stays last, unchanged, guarding only the frame shape.
- **The cost is accepted and surfaced, not hidden.** After any rebuild, the running daemon stops serving until restarted — Direct fallback is always correct (trust over speed), the one-line note names the fix, and `restart pending (rebuilt)` shows in status. `norn self-update` restarting the loaded service is tracked separately.

## Consequences

- Wire-schema growth is a non-event: additive or breaking tool-param and report-shape changes need no per-field required-bit policy, no CONTROL_PROTOCOL bump, and no migration shim for the daemon path — a skewed pair simply never routes.
- Dogfooding between rebuilds runs Direct until the daemon restarts. This trades warm-cache speed for byte-trust, per the northstar.
- Anything that changes runtime behavior WITHOUT touching `src/` or `Cargo.lock` (e.g. a future feature-flag or env-dependent wire shape) would evade the fingerprint — keep wire-affecting configuration out of that blind spot, or fold it into the hash inputs.
## Amendment — 2026-07-17: the fingerprint demotes from gate to address

Under [0017](./0017-registered-vaults-summoned-owners.md), daemon sockets are **keyed by build fingerprint**, so a client can never connect to a mismatched daemon — the address embeds the identity, and the handshake comparison demotes from routing decision to sanity assert. Consequences: N worktree agents developing N different builds simultaneously each summon their own ephemeral owner on their own socket with their own tmp cache — concurrent multi-build development becomes a non-case rather than a managed one. `norn self-update` mints a new build-id → new socket; the old owner idles out orphaned and the first post-update invocation summons fresh — the `restart pending (rebuilt)` status semantics this ADR pinned are deleted with the condition they described. Source-content blake3 hashing (not timestamps) carries forward as the fingerprint definition.

## Amendment — 2026-07-23: the standing fingerprint is exe-identity, not the `src/`-tree hash — for the ephemeral tier

This ADR's fingerprint definition (`build.rs` emitting `NORN_BUILD_ID` from a blake3 over the sorted `src/` tree + `Cargo.lock`) has no build script implementing it. `build_fingerprint` (`crates/norn-client/src/addr.rs`) instead hashes **executable identity at runtime** — blake3 over `current_exe()`'s path, byte size, and mtime (`current_exe_identity`) — truncated to the socket's fixed-width fingerprint segment. This satisfies the one load-bearing property the 2026-07-17 amendment's socket-as-address needs for the ephemeral tier: different builds get different fingerprints, and the client and the owner it spawns hash the same file, so they agree on one fingerprint. It diverges from the `src/`-tree definition in one way the ephemeral tier accepts: a no-op relink (same source, new mtime/inode) mints a new fingerprint and a new socket, orphaning the old owner to idle out — cosmetically wasteful, never incorrect, and already the accepted cost model for a rebuild under the prior amendment.

**NRN-358 remains the open obligation**: the real source-content hash this ADR specifies is still owed before a resident tier (one long-lived daemon serving multiple builds, or any cross-host/cross-checkout identity claim) can rely on the fingerprint for more than same-process addressing. Exe-identity is a standing, adequate substitute for the ephemeral tier only — it is not a rename of this ADR's decision, and this amendment does not close NRN-358.
