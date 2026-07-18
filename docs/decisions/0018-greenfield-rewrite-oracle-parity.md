---
title: "0018 — Greenfield rewrite on a long-lived branch, anchored by an oracle parity harness"
description: "Architectural decision that the 0017 rewrite executes greenfield-in-place on a long-lived integration branch, with the existing source retired to documentation-only status, correctness anchored by an oracle parity harness against the prior release, divergence permitted only through a decision-gated ledger, and main remaining the stable release line until switchover."
---

# 0018 — Greenfield rewrite on a long-lived branch, anchored by an oracle parity harness

**Decision:** the registered-vault rewrite ([0017](./0017-registered-vaults-summoned-owners.md)) executes **greenfield-in-place on a long-lived integration branch** (`rewrite/0017`), not by incremental refactor of the live tree. The existing `src/` moves to `retired/` — excluded from CI, not building, demoted to documentation, with the porting burn-down list inside it as the single tracker of what remains to move. New crates are built fresh in a workspace layout; outward interfaces (CLI grammar, MCP catalog, config semantics, output shapes) are ported first as 1:1 design constraints; core logic is ported organ-by-organ until `retired/` is empty and deleted. `main` remains the stable release line — releases and self-update artifacts are cut only from `main` until one switchover PR lands the rewrite. Correctness during the rewrite is anchored by an **oracle parity harness**: the pinned released binary and the rewrite binary run against generated fixture vaults and their outputs are compared mechanically, with divergence permitted only through a decision-gated ledger.

## Context

The previously committed migration stance ("code that predates the crate lines is moved into its crate as it is touched") kept the old system usable throughout the rewrite — and thereby kept the old architecture in every agent's context as something to maintain, exerting gravitational pull toward the way things used to work. The task backlog showed the symptom: the early rewrite phases were dominated by refactors of existing command code, shaped by what the old tree made easy rather than by the target architecture.

Separately, full integration testing depended on a single private real-world vault that contributors and CI cannot reproduce (and which already breaks in CI). Any rewrite verification strategy built on that vault would inherit the same problem.

## The trade

- **Rejected: incremental strangler in the live tree.** Safer per-step and continuously usable, but each step is shaped by the old code's structure, the old system must stay in context as a source of truth, and "usable throughout" was a constraint the project does not currently need — there are no external consumers requiring an unbroken binary from this branch.
- **Accepted: greenfield with a lit dark period.** The rewrite branch is not usable mid-flight and real-vault dogfooding softens until graduation. In exchange, the new system is shaped only by the target architecture plus the interface contract — and the parity harness makes "interfaces preserved 1:1" *executable* instead of aspirational: a surface counts as ported when its parity suite passes against the oracle.

## Mechanics

- **Parity oracle:** the pinned released binary is the executable specification. The parity runner drives both binaries with the same argv (and the same MCP frames) against deterministic, seed-driven **fixture vaults**, comparing stdout, stderr, exit codes, JSON shapes, and help text under output normalization.
- **Divergence ledger:** every intended old→new difference is recorded — surface, both behaviors, and the reason (a decision that the new behavior is more correct, or a discovered inconsistency between old commands). The runner has exactly three verdicts: **match**, **diverged-with-ledger-entry** (passes, citing the entry), and **drift** (fails). There is no fourth state.
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
