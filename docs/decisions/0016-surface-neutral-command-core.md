---
title: "0016 — Surface-neutral command core: adapter → dispatch → execute, parity by construction"
description: "ADR: one surface-neutral execute(env, Params) -> Report seam per command; MCP handler layer promoted to core; adapters stay thin; daemon is a host; parity enforced by construction and finished by the crate split"
---

# 0016 — Surface-neutral command core: adapter → dispatch → execute, parity by construction

**Decision:** Every norn command runs through one surface-neutral seam: per-verb `execute(env, Params) -> Report`, reached by every surface in the same three beats — **adapter → dispatch → execute(env)**. The existing MCP handler layer is promoted to be that seam: its params structs are the canonical Request vocabulary, the typed report structs the canonical Report vocabulary. clap, rmcp framing, and any future HTTP or napi surface are thin adapters (parse and present only); the serve daemon is a **host** for `execute` behind a socket, never a layer that re-routes. Surface parity is held **by construction, finished by the compiler**: the capstone workspace crate split (`norn-core` / `norn-cli` / `norn-mcp` / `norn` bin) makes "core references clap or rmcp" a compile error. Design of record: *the command-core-seam design doc (internal design doc)*.

## Context

The 2026-07-16 architecture review converged on one diagnosis: norn has a shared leaf core but no shared orchestration core. The lock → preflight → plan → apply → report sequence was hand-written twice per command (the `lib.rs` dispatch arms and each `mcp/tools/*::handle`), plus a cfg-doubled routing seam — held consistent by discipline and a parity-test tax (~42% of the suite). The failure mode is not hypothetical: the CLI and MCP refusal envelopes drifted (`norn new --format json` emits prose where `vault.new` returns the structured envelope), and `mcp/tools/new.rs` documents in comments that it must re-assemble orchestration it cannot reuse. Externally, the same disease broke an external consumer whose hand-mirrored plan types met norn 0.48's plan v2 requirement at runtime.

The enabling facts, verified in-code before deciding: all 14 MCP tools already expose pure `handle(ctx, scope, params) -> Result<Report>`; the report types already serialize byte-identically to the CLI's `--format json` output; the serve daemon already answers through the MCP handlers (two implementations plus a transport, not three); and ADR 0009's carve-outs (safety model, presentation, exit signal, local-only commands) all resolve *outside* the proposed seam.

## The load-bearing choices

- **Bless the existing vocabulary; no second one.** Params = Request (three-form identity with the CLI token grammar per ADR 0010 — one coercion path, including `KEY=VALUE`/`KEY=JSON` tokens, now a permanent choice); typed reports = Report. Inventing a parallel "cleaner" Request layer would recreate the parallel-paths disease inside the fix.
- **One router per request; the daemon is a host.** Dispatch answers exactly one question: cold in-process env, or the warm daemon over the socket (ADR 0012's fingerprint gate handles skew). The socket's remote end deserializes and executes — no re-dispatch. The wire stays MCP-shaped, so HTTP later is "expose the same server over HTTP".
- **Env from process lifetime, not surface.** `VaultEnv::cold()` for one-shot invocations; the warm generational pool (ADR 0013, unchanged) for resident processes. Consequence: stdio `norn mcp` routes through a live daemon like every other entry — one warm cache per vault, machine-wide.
- **Typed-primary; tokens desugar.** Inside core, the typed operation layer is the primary path; the token grammar is a front-end that desugars into it, never the only way in. This is what keeps the engine embeddable (napi later) without a rewrite.
- **The dispatcher lives in `norn-core`,** so any embedder inherits routing, fallback, and skew-gating for free.
- **Consumer coupling is elective, not forced.** Consumers pin a norn version (bundled binary + types generated from norn's published schemas) and upgrade when they choose. Today's coupling is implicit and forced — `norn self-update` swaps the dependency under every consumer at once — which is the root of that failure class. Requires norn to publish real schemas, including typed `MigrationPlan` op payloads (untyped `fields` today).

## Alternatives rejected

- **A new surface-neutral Request/Report vocabulary distinct from the MCP types.** Rejected: the MCP types already are that vocabulary (designed as the CLI's own shapes); a second one means two vocabularies to hand-sync — the disease again.
- **A `CommandSpec` runtime dispatch table** (the review's original sketch). Rejected as machinery: the daemon-side registry already exists (the rmcp router); the CLI side needs only a small `Request` trait + one generic dispatch function.
- **Always-route-to-daemon (kill the cold path).** Rejected: the daemon stays an optional accelerator (ADR 0005 posture); the cold path is now budget-compliant and must work with no daemon installed.
- **Keep parity-by-test.** Rejected by evidence: the gate self-documents that it cannot catch semantic divergence, and the drift happened anyway. Tests police copies; construction removes them.

## Consequences

- **Adapter-seam maintenance contract:** a new command is written once — `params` + `execute` + `report` in its verb module — and every surface gets it. A capability-adding PR no longer touches ~6 sites; the parity obligation of ADR 0009 is discharged structurally. ADR 0009's doctrine (peer surfaces, carve-out allowlist) is unchanged; its **enforcement mechanism is amended**: the parity gate demotes to a schema-shape check, and the byte-identity routing suites collapse to one wire round-trip test once a verb is on `execute`.
- The existing byte-identity suites remain the safety net *during* migration and demote only after.
- `VaultEnv` becomes the mock seam for unit-testing handlers without binary spawns (with a dedicated handler-test harness).
- The clap-free core crate work is superseded — absorbed as the capstone phase. The error-contract work rides the same wave: one `CodedError` refusal seam inside `execute` serves both surfaces.
- MCP output envelopes become fully typed (retiring the `serde_json::Value` wrapper wart), making the published schemas the real contract surface.
- Costs accepted: a multi-release migration (~v0.49–v0.51); behavior-fidelity risk while carving TTY/stdin out of the fused verbs (edit, new, rewrite_wikilink); a churn spike at the crate split, sequenced last for that reason.
- This decision flips only if the surfaces stop sharing semantics — i.e., if a surface ever *wants* divergent behavior beyond ADR 0009's carve-outs, which would be a mission change, not a refactor.
## Amendment — 2026-07-17: dispatch's question changes; the cold path is un-rejected into deletion

[0017](./0017-registered-vaults-summoned-owners.md) reverses this ADR's rejection of "always-route-to-daemon" (what changed: trust moved into the owner, so a non-routed path is no longer a cheaper equivalent but an untrusted parallel implementation). The seam itself — adapter → dispatch → execute(env, Params) → Report, MCP shapes as the canonical vocabulary, parity by construction via the crate split — carries forward as the spine of the rewrite. Dispatch's one question becomes "is this vault's owner resident, or must it be summoned" (never "routed or direct"); `VaultEnv::cold()` is deleted with the cold path. Module and crate boundaries for the target system are drawn in *the target-architecture-boundaries design doc (internal design doc)*.
