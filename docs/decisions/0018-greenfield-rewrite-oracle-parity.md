---
title: "0018 — Greenfield rewrite on a long-lived branch, anchored by an oracle parity harness"
description: "Architectural decision that the 0017 rewrite executes greenfield-in-place on a long-lived integration branch, with the existing source retired to documentation-only status, correctness anchored by an oracle parity harness against the prior release, divergence permitted only through a decision-gated ledger, and main remaining the stable release line until switchover."
---

# 0018 — Greenfield rewrite on a long-lived branch, anchored by an oracle parity harness

**Decision:** the registered-vault rewrite ([0017](./0017-registered-vaults-summoned-owners.md)) executes **greenfield-in-place on a long-lived integration branch** (`rewrite/0017`), not by incremental refactor of the live tree. The existing `src/` moves to `retired/` — excluded from CI, not building, demoted to documentation, with the porting burn-down list inside it as the single tracker of what remains to move. New crates are built fresh in a workspace layout; outward interfaces (CLI grammar, MCP catalog, config semantics, output shapes) are ported first as the pinned end-user contract; core logic is reimplemented organ-by-organ to that contract until `retired/` is empty and deleted. `main` remains the stable release line — releases and self-update artifacts are cut only from `main` until one switchover PR lands the rewrite. Correctness during the rewrite is anchored by an **oracle parity harness**: the pinned released binary and the rewrite binary run against generated fixture vaults and their outputs are compared mechanically, with divergence permitted only through a decision-gated ledger.

## Context

The previously committed migration stance ("code that predates the crate lines is moved into its crate as it is touched") kept the old system usable throughout the rewrite — and thereby kept the old architecture in every agent's context as something to maintain, exerting gravitational pull toward the way things used to work. The task backlog showed the symptom: the early rewrite phases were dominated by refactors of existing command code, shaped by what the old tree made easy rather than by the target architecture.

Separately, full integration testing depended on a single private real-world vault that contributors and CI cannot reproduce (and which already breaks in CI). Any rewrite verification strategy built on that vault would inherit the same problem.

## The trade

- **Rejected: incremental strangler in the live tree.** Safer per-step and continuously usable, but each step is shaped by the old code's structure, the old system must stay in context as a source of truth, and "usable throughout" was a constraint the project does not currently need — there are no external consumers requiring an unbroken binary from this branch.
- **Accepted: greenfield with a lit dark period.** The rewrite branch is not usable mid-flight and real-vault dogfooding softens until graduation. In exchange, the new system is shaped only by the target architecture plus the interface contract — and the parity harness makes "the end-user contract is preserved" *executable* instead of aspirational: a surface counts as ported when its parity suite passes against the oracle.

## Mechanics

- **Parity oracle:** the pinned released binary is the executable specification of the **end-user contract** — its observable behavior at the boundary (stdout, stderr, exit codes, JSON shapes, help text). The parity runner drives both binaries with the same argv (and the same MCP frames) against deterministic, seed-driven **fixture vaults**, comparing those outputs under normalization. The oracle is a **black-box drift detector, never a template**: it pins what a caller observes, not how the code that produces it is written.
- **Porting means reimplementing to that pinned contract inside the new boundaries.** Transcribing donor source is a failure mode, not the method — the retired tree is a semantics-and-edge-case reference, not text to copy. Preservation is **conditional**: what carries forward is good behavior and valid user expectations, and only because a parity case pins it — not the donor's internal shape. Diffing new code against donor lines is **banned as a review pass-signal**; a surface counts as ported when its parity cases pass, never because it resembles the donor. This rule has **equal standing with the parity requirement itself**.
- **Divergence ledger:** every intended old→new difference is recorded — surface, both behaviors, and the reason (a decision that the new behavior is more correct, or a discovered inconsistency between old commands). The runner has exactly three verdicts: **match**, **diverged-with-ledger-entry** (passes, citing the entry), and **drift** (fails). There is no fourth state. A **defect discovered while porting is never dismissed** — it terminates as fixed-in-place with a ledger entry, or is deferred to a dedicated tracked task; "the old code did it this way" is not a disposition.
- **Fixture-vault generator:** deterministic test-vault generation (schema variety, link topologies, frontmatter edge cases) replaces reliance on any single private vault; it is independently justified by the contributor/CI reproducibility gap and outlives the rewrite as shared test infrastructure.
- **Branch enforcement is structural:** a CI job fails any pull request targeting `main` from a `rewrite/**` head branch; the branch policy is also carried in the repository agent instructions and on the rewrite initiative in the work tracker. The single graduation switchover PR is the deliberate, documented exception: it lands from a dedicated switchover branch (not `rewrite/**`) cut at graduation, so the guard stays absolute without blocking its own endgame.
- **Crate lines are earned:** a crate boundary requires a plausible standalone future, an effect boundary the compiler must hold, or isolation of a heavy dependency — otherwise it is a module. This adds `norn-wire` (the pure-types request/response vocabulary, so client-side crates never depend on the core) and `norn-frontmatter` (the text layer, with a standalone-library future) to the crate map. All workspace crates are `publish = false`; distribution remains release binaries only.
- **Always-routed from the first command; new-world after parity.** The summoner and ephemeral owner exist before the first read verb works — no in-process compatibility hedge is ever built. Ported surfaces are proven against the oracle before the surfaces with no oracle (registration, trust flip, continuous GC) are built.

## Graduation

The rewrite replaces the stable line only after: the full parity suite is green, the divergence ledger is fully adjudicated (zero drift verdicts), beta prereleases cut from the rewrite branch (marked prerelease; never served to the stable update channel) have been in daily real-vault use, and the betas stop surfacing findings. Then one switchover PR lands the rewrite on `main`, `retired/` is deleted, and the parity suite converts into the standing release-to-release regression harness.

## Consequences

- The harness (generator + parity runner + ledger) is built first, before anything user-visible.
- `main` accepts critical fixes only once the trees diverge structurally; fixes needed by both lines are hand-ported. No continuous two-way merging.
- Real-vault dogfooding is suspended as a daily practice on the rewrite branch and re-enters as the graduation test.
- The ADR corpus and glossary live in this repository (this directory) as the authoritative home, so agents working the rewrite carry the decision context in-repo.
- **Flips if:** the parity harness proves unbuildable at acceptable cost, or the dark period starts accumulating unverified surface anyway — in which case fall back to the incremental stance this ADR supersedes.

## Amends

- [0017](./0017-registered-vaults-summoned-owners.md) — execution assumptions (the *what* is unchanged).
- The target-architecture boundary contract — crate table gains `norn-wire` and `norn-frontmatter`; the touch-by-touch migration stance is replaced by this ADR's model.

## Amendment — 2026-07-21: the contract-type boundary rule

The rewrite settled the crate boundary for end-user contract types, recorded here as the standing rule (aligned with `docs/architecture.md` principle 1):

- **End-user contract types live in `norn-wire`.** Every shape a caller reads — plans (`MigrationPlan` and its ops/preconditions), reports (`ApplyReport`), and the warning/error envelopes — is a pure serde struct in `norn-wire`. `norn-core` constructs the values; the client and surface crates render them. `norn-wire` carries no logic and no I/O beyond the pure data/serde methods these types own.
- **Surface and client crates never depend on `norn-core`.** They depend on the `norn-wire` vocabulary, not the engine. This keeps "the client never opens a cache" compile-enforced.
- **A crate edge must carry real code.** A dependency declared only to reference another crate's `CONTRACT` const — with no type or function actually used across it — is a manifest lie and comes out. The `norn-mcp → norn-core` edge was exactly such a CONTRACT-const-only edge and was removed.

An engine method that needs `norn-core` internals cannot ride on a `norn-wire` type; it becomes a free function or extension in `norn-core` (the coded-error envelope constructors are the example).

## Amendment — 2026-07-23: live-tree comments speak in present-tense code facts

The parity harness, the divergence ledger, and the `retired/` donor tree are load-bearing *during* the rewrite — but they are point-in-time validation instruments, not durable authorities a reader of the live tree needs. Recorded here as the standing doctrine:

- **A comment states the constraint or fact in present-tense code-and-principle terms** — what the contract IS and why it holds — never who or what once validated it. "This field set IS the wire contract (the `plan_hash` is its `canonical_hash()`)" earns its place; "pinned by the delete plan parity case" and "byte-identical to the donor" do not.
- **Provenance belongs to git history and ADRs.** ADR references are durable decision records and stay. Harness / oracle / donor / ledger / `retired/`-tree citations, and PD-ledger ids used as rationale, do not — they answer "who once checked this?", a question git and ADRs already answer. A task id survives only where it marks genuinely pending work, never as historical attribution.
- **Byte-identity framing is banned even as a factual description.** A clean refusal leaves the vault *unchanged*; two paths produce *identical* output. Say that.
- **Reviews enforce it; the guard test is the forcing function.** `crates/norn-cli/tests/comment_truth_guard.rs` scans every live crate's `src/` for the authority needles and fails on a match, with an explicit (and, by design, empty) allowlist for genuinely operational references. The one-time sweep that brought the tree to zero is NRN-450.

This doctrine flips at graduation only in that `retired/` and the harness cease to exist; the present-tense rule for live-tree comments is permanent.
