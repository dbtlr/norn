# norn — working principles for Claude

Project-level instructions for agents working in this repo. Workspace-private setup details live in `CLAUDE.local.md` (gitignored). This file is the durable middle layer: norn-specific habits learned from real CI failures and Drew's stated design constraints.

## Northstar

**norn is the agent's query primitive, not a stage in a pipeline.** Every output / filter / sort / limit / paging / column-selection decision must hold against this. When the instinct says "agents can just pipe to jq for this," push back — norn should do it natively. Filter, sort, limit, paging, column selection, and (eventually) grouping all native by default.

Drew named this during the `norn find` brainstorm: *"prevent agent piping and turns where possible… we can't stop it completely day one, but that is a long term northstar."* It's the design constraint that should shape every new command and every output format choice.

## Per-task verification (Rust workspace)

CI runs `cargo test --workspace --locked`. The per-task verification step must include ALL four of these — gaps here have failed CI multiple times. **Order matters**: run the `--locked` check FIRST so any `Cargo.lock` drift surfaces before the non-locked test command masks it by silently regenerating the lockfile.

```
cargo check --workspace --locked
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
git status --short   # Cargo.lock should NOT show as modified
```

The first command catches `Cargo.lock` drift (locally `cargo build` and `cargo test` regenerate the lockfile silently; CI rejects with `--locked`). The trailing `git status` is a belt-and-suspenders check — if anything in the quartet still regenerated the lockfile (rare but possible on dep-tree shifts during rebase), `Cargo.lock` shows as modified and you commit the regeneration as part of the change. The fmt-check belongs in verification even when clippy is clean (fmt and clippy enforce different rules).

For subagent dispatch prompts, include all five explicitly. Don't trust the implementer's verbal "tests pass" — ask for the raw `cargo test --workspace 2>&1 | grep "test result"` lines and have the spec reviewer independently re-grep and sum.

## Design framing for command redesigns

**Start from jobs-to-be-done, not the existing command surface.** The current commands are the *output* of past design decisions, often historical-accident-shaped. Drew has corrected me twice when I led with "here are the 21 commands, how do they group?" — the right opener is "who is calling this, what are they trying to accomplish, what would they pipe it into next?"

The flow:
1. List the jobs (humans + agents + pipelines).
2. Ask whether today's commands serve those jobs cleanly.
3. Derive the rename / merge / remove / add set.

Surface enumeration first locks the conversation into "how can these commands work better?" instead of "should these commands exist at all?"

## Pre-release posture

norn is pre-1.0. No external consumers besides Drew. Drew has been explicit: *"this is all pre-release. There are no consumers outside of me at the moment. Now is the time for churn to exist."*

When CI failures or downstream tests surface a v1-parity gap during a redesign, the default response is **redesign, not restore.** Question whether the existing contract was deliberate or historical-accident-shaped; prefer breaking changes (with CHANGELOG breaking-change entries) over preserving suspect behavior. This flips post-1.0 — but until then, churn is cheap.

## Dogfood against a representative real vault

Every shipped command runs against a representative real-world Markdown vault before merge — both for correctness (output shape, exit codes) and for timing (the 50ms target for typical queries). Don't ship if a command exceeds the perf budget on real-vault scale data, or surface the regression deliberately in the CHANGELOG if it's intentional.

`EXPLAIN QUERY PLAN` against `documents` / `links` / `headings` is the standard tool for diagnosing slow queries. Guard tests verify the plan stays a single SCAN/SEARCH (no per-row sub-queries). The specific dogfood vault path and current baseline numbers are in `CLAUDE.local.md`.

## Subagent dispatch patterns

When dispatching a TDD-shaped subagent:

- **Include the anti-pattern callout:** *"Do NOT silently change test assertions to make tests pass. If a test fails because of a real semantics issue, stop and report DONE_WITH_CONCERNS."* This sentence has caught real plan bugs twice — implementers correctly investigated rather than fudged when given the explicit instruction.
- **Request raw output, not summary:** ask for `cargo test --workspace 2>&1 | grep "test result"` verbatim lines, not a verbal sum. Subagent counts have been wrong multiple times; independent re-counting catches the drift.
- **Combine spec + quality review for mechanical tasks.** Skip the separate reviewer for purely mechanical changes (renames, stub additions, format-only renderers); keep them separate for tasks touching multiple files or making real design decisions.

## Spec self-review is load-bearing

After writing any design spec, run the brainstorming-skill's self-review pass (placeholder / consistency / scope / ambiguity sweep) **before** Drew reviews it. This pass has caught real defects in every spec where it was applied this week:

- Cache v2 spec: per-row sub-query trap (the headline perf bug); JSON-path injection-vector over-restriction; globset-vs-pattern_matches_path mismatch; repair-plan two-phase shape implicit; exit-code primitive gap.
- Find spec: `--col` paths-format ignored-warning missing; `--text ""` semantics ambiguous; truncation footer suppression unspecified.

Without the self-review, these defects ship into the plan and the implementation. The 30 minutes the self-review takes saves multiple subagent round-trips downstream.

## Design constraints Drew has named

These are durable preferences across sessions. Honor them when they apply:

- **The `docs` namespace is dead.** Drew: *"I hate the docs commands, their naming is unintuitive."* Any new command must use a job-shaped name (`find`, `links`, `validate`), not a noun-shaped one (`docs`, `files` is borderline). When `norn docs summary` and `norn docs inspect` get their redesign turn, they need new names.
- **Records output, not tables.** Drew: terminal rendering of query results is per-doc key-value blocks with terminal-width-aware value wrapping. The reference is pgcli / mycli vertical mode, not a spreadsheet grid. Don't reach for column-style tables for multi-field output.
- **Default to dump-everything; let users narrow.** Drew: *"Without it, dump everything. Let the user / agent ask for less, they might not know what that is until they see it and then filter down."* `--col` and similar narrowing flags are subtractive; the default shows everything.
- **`warn`, don't `block`.** For non-destructive operator decisions, norn warns and proceeds; blockers reserved for cases where the action can't proceed cleanly.

## Three-layer durability for shipping

When shipping any non-trivial work:

1. **CHANGELOG `## [Unreleased]`** — operator-facing summary of every user-visible change (per the `changelog` skill).
2. **Squash commit body** — preserve per-task SHAs + design highlights + mid-execution catches inline. `git log -1 <sha>` recovers the per-task history that squash deletes.
3. **External design archive** — design spec, implementation plan, dev log. Drew's specific archive location is in `CLAUDE.local.md`.

Each layer answers a different question (what shipped / what's the code history / why was it built this way). Don't duplicate effort across them.

## Worktree workflow

Drew prefers isolated worktrees for substantial multi-commit work. Use the native `EnterWorktree` tool — not `git worktree add` directly. The harness needs to see the worktree state; `git worktree add` creates phantom state it can't see or manage.

`EnterWorktree` branches from `origin/main`. Always check that local main is in sync with origin before creating a worktree, and rebase if local is ahead.
