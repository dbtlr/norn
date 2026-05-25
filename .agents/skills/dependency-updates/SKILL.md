---
name: dependency-updates
description: How to evaluate, migrate, and ship cargo dependency updates in vault-cli — including dependabot PRs and manual bumps. Covers triage, local verification, breaking-API migration shapes, CHANGELOG conventions, and merge mechanics (squash, conflict resolution, force-push to dependabot's branch). Use when a dependabot PR is open, when CI fails on a dep bump, when evaluating whether to take a bump now, or when staging a manual cargo update.
---

# Dependency updates in vault-cli

## Why this exists

Deferred breaking-API bumps compound. A single migration (one call site, demonstrable byte-equivalence) is easy now; bundling it with the next two cargo-API breaks ships a riskier multi-front migration later. The principle: *tackle dep bumps when they appear, don't accumulate them.*

This skill is the procedure. The preference itself lives in `feedback-keep-deps-fresh.md`.

## The flow

```
[ dependabot PR opened ]  or  [ manual cargo update ]
              │
              ▼
   1. TRIAGE: what is this? (runtime/dev, patch/minor/major)
              │
              ▼
   2. LOCAL VERIFY: full quartet against current main
              │
        ┌─────┴─────┐
   clean?         not clean?
        │             │
        ▼             ▼
   3a. CHANGELOG +   3b. ASSESS BREAKAGE
       merge              │
                          ▼
                     4. CHOOSE SHAPE (A/B/C)
                          │
                          ▼
                     5. MIGRATE + verify + CHANGELOG + merge
```

## 1. Triage

For every dep update — dependabot PR or local bump — answer four questions before doing anything else:

| Question | Why it matters |
|---|---|
| Is this a runtime dep (`[dependencies]`) or a dev dep (`[dev-dependencies]`)? | Runtime → lands in the user's binary → CHANGELOG entry. Dev → no effect on the shipped artifact → no CHANGELOG. |
| Patch / minor / major bump? | Patch is almost always safe; minor is usually safe; major usually breaks something. |
| Does the dep produce code that ends up linked in the binary? | "yes" means it counts as binary-affecting (the binary-effect test from the `changelog` skill applies). |
| What is the dep's role in the codebase? | (e.g., HTTP client vs. crypto vs. CLI parser) — informs the blast-radius search if breakage shows up. |

For dependabot PRs, also check:

```bash
gh pr list --state open --author "app/dependabot" --json number,title,headRefName,mergeable,mergeStateStatus
gh pr checks <pr-number>
```

`mergeable: MERGEABLE` and `mergeStateStatus: CLEAN` + green CI is the green-light state. Anything else needs investigation.

## 2. Local verify

**Never trust the dependabot-side CI alone.** Reasons:

- Main may have moved since dependabot rebased; the GH-side CI ran against an older base.
- Local toolchain quirks can surface things CI didn't catch (rare but real).
- You need a local checkout anyway to push a CHANGELOG entry (and migration commits if needed).

Procedure:

```bash
git fetch origin
git checkout -b dependabot-pr-<n> origin/dependabot/cargo/<dep>-<version>

# IMPORTANT for dep updates: run --locked FIRST. The non-locked `cargo test`
# silently regenerates Cargo.lock, masking lockfile drift that CI catches
# under --locked. Order matters.
cargo check --workspace --locked

# Then the rest of the quartet:
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check

# Finally, confirm the lockfile is still clean (in case test regenerated it):
git status --short  # Cargo.lock should NOT show as modified
```

If `git status` shows `M Cargo.lock` after the quartet, you have lockfile drift — commit the regenerated Cargo.lock as part of the dep PR (`build: regenerate Cargo.lock after <dep> bump`). CI runs `cargo test --workspace --locked` and will fail on stale lockfiles.

Note: `cargo deny check` is NOT in the quartet but IS in CI. License-allowlist failures show up only in CI. If you've added a new dep that pulls in an unfamiliar license (e.g., `webpki-roots` ships under `CDLA-Permissive-2.0`), expect a CI failure unless `deny.toml` already has the license.

## 3a. Clean bump — CHANGELOG + merge

If the quartet passes:

1. Add a one-liner under `### Changed` in `## [Unreleased]` (per the `changelog` skill). Examples:
   - Patch bump: `Bumped \`serde_json\` 1.0.149 → 1.0.150 (patch).`
   - Minor bump with bundled native code: `Bumped \`rusqlite\` 0.32.1 → 0.39.0 (still \`features = ["bundled"]\`; ships with a newer bundled SQLite). No source changes required; cache schema and on-disk format unchanged.`
2. Commit on the dependabot branch:
   ```bash
   git add CHANGELOG.md
   git commit -m "docs(changelog): note <dep> <old> → <new> bump"
   git push origin dependabot-pr-<n>:dependabot/cargo/<dep>-<version>
   ```
3. Wait for CI to re-run, then merge:
   ```bash
   # Wait for CI:
   until [ "$(gh pr checks <n> 2>&1 | grep -cE '^ci.*\b(pass|fail)\b')" -eq 2 ]; do sleep 20; done
   gh pr checks <n>

   # Merge:
   gh pr merge <n> --squash --delete-branch
   ```
4. Pull main locally, delete the local branch:
   ```bash
   git checkout main && git pull --ff-only origin main
   git branch -D dependabot-pr-<n>
   ```

## 3b. Not clean — assess breakage

If the quartet fails on the dep branch, the dep ships a breaking change. Read the CI/local error and classify:

| Failure shape | Likely cause |
|---|---|
| Trait bound `…: LowerHex` not satisfied | Type changed; formatter no longer works (sha2 0.11 case) |
| Method `…` not found | API rename or removal |
| Cannot find type `…` in crate | Type re-exported elsewhere or moved |
| MSRV violation | Crate raised its rust-version requirement |
| License rejection (cargo-deny) | Add license to `deny.toml` allow-list if permissive; otherwise reject the bump |

Then find call sites:

```bash
# Examples — replace with the actual API surface:
grep -rn "use <dep>::\|<dep>::" crates/ --include="*.rs" | grep -v target
grep -rn "format!(\"{:x}\"" crates/ --include="*.rs"  # for LowerHex breaks
```

Blast-radius questions:
- How many call sites are affected?
- Are they all in the same crate?
- Is the migration obvious (mechanical) or judgment-heavy (semantic)?
- Is there a load-bearing invariant the migration must preserve (e.g., cache identity)?

## 4. Choose migration shape

Three shapes, picked by risk vs. ceremony:

### A. Single PR — bump + migrate

Add the call-site fix(es) on the dependabot branch, alongside dependabot's `Cargo.toml` bump. One atomic merge.

**Use when:** the migration is small (one or two call sites), demonstrably equivalent, and the surface area is contained.

**Trade-off:** if the migration has a subtle bug, the bump is harder to revert cleanly (since the migration commits live with the bump in one PR).

### B. Two PRs — migrate first, bump second

PR 1: refactor the call sites to the new idiom against the *old* dep version (e.g., switch from `LowerHex` formatter to byte iteration on sha2 0.10). This must be a no-op (tests pass, output identical). PR 2: the dep bump itself, which is now a trivial compile-fix.

**Use when:** the migration is big enough that you want it reviewed/landed in isolation before the bump, or when the equivalence is non-obvious and you want CI on old + new versions to prove the refactor.

**Trade-off:** two PRs, two merges, slightly slower.

### C. Reuse dependabot's PR

Push the call-site fix to dependabot's branch (same as A, but the PR is dependabot's, not yours). Dependabot's bookkeeping then sees its PR merged and stops reproposing the same bump.

**Use when:** you're handling the bump promptly and the migration is small.

**Pitfall:** if you've previously closed the dependabot PR, GitHub may refuse to reopen it via API (`Could not open the pull request`). Workaround: push to the same branch and open a fresh PR pointing at it — dependabot will infer the merge from the eventual commit on main and stop proposing the version.

**Recommendation:** prefer C when the PR is still open; fall back to A (fresh PR from the same branch) if the dep PR was closed.

### Concrete trade-off prompt to use with Drew

When the migration is non-trivial, surface the three shapes explicitly before picking one. Example shape:

> Three options: **A.** Single PR bump+migrate. **B.** Two PRs (refactor first, then bump). **C.** Reuse dependabot's branch (same as A but via the existing PR). My lean is **C** because [reason]; A is the fallback if reopening fails; B is overkill here because [reason]. Which?

## 5. Migrate + verify + CHANGELOG + merge

After picking a shape:

1. Implement the migration.
2. **Add a pin-test for any load-bearing invariant** the migration must preserve. Cache-identity hashes, on-disk file formats, sort orders, serialization stability — anything where a future regression would silently corrupt user data. Pin a known input to a known output so future changes get caught.
   ```rust
   #[test]
   fn hash_format_pins_lowercase_no_separator_hex() {
       // ... compute hash of fixed input, assert against precomputed sha256 hex
   }
   ```
3. Run the full quartet locally — must be clean before pushing.
4. Add a CHANGELOG entry under `### Changed` that describes both the bump AND the migration (the migration is the user-relevant part):
   > Bumped `<dep>` `<old>` → `<new>`. <what changed in the API>; <what we did to migrate>. <invariant preserved>.
5. Commit, push, wait for CI, merge.

If multiple dep PRs are landing in sequence, the second one's CHANGELOG will conflict on the `### Changed` block. Resolve by keeping both bullets in order:

```bash
git checkout dependabot-pr-<later>
git rebase origin/main
# Edit CHANGELOG.md to combine both bullets under ### Changed
git add CHANGELOG.md
git rebase --continue
git push --force-with-lease origin dependabot-pr-<later>:dependabot/cargo/<dep>-<version>
```

## Pitfalls

### Pitfall: trusting GH-side CI on an old base

Dependabot's PR CI ran against whatever main was when the PR was opened. If main has since changed (e.g., a new dep was added, deny.toml was updated), the GH CI status doesn't reflect the merged state. **Always verify locally** with the full quartet.

### Pitfall: skipping the pin-test on load-bearing migrations

Without a pin-test, a future formatter change can silently produce a different byte sequence for the same input. For cache-identity hashes, that means orphaned cache directories. For on-disk schemas, that means data loss. The pin-test is cheap; the regression it catches is expensive.

### Pitfall: closing a dependabot PR you actually want to revive

Once closed, GH may refuse `gh pr reopen <n>`. If you anticipate doing the migration soon, leave the PR open with an explanatory comment instead of closing.

### Pitfall: cargo-deny only runs in CI

A new TLS-touching dep often pulls in `webpki-roots` (CDLA-Permissive-2.0) or similar permissive licenses not in vault-cli's existing allow-list. The local quartet is silent on this. Watch the first CI run on a new-dep PR.

### Pitfall: lockfile drift masked by `cargo test --workspace`

The non-locked `cargo test --workspace` (and `cargo build`) silently regenerates `Cargo.lock` if the dep tree resolves differently than what's recorded. CI runs `cargo test --workspace --locked`, which rejects any drift. Common trigger: rebasing a dep PR onto a new main pulls fresh transitive resolutions that the bump's original `Cargo.lock` doesn't match.

**Symptoms:** Local quartet green, CI fails with `the lock file Cargo.lock needs to be updated but --locked was passed`.

**Prevention:** Run `cargo check --workspace --locked` *before* the non-locked test commands so any drift surfaces immediately. After the quartet, `git status` should show no Cargo.lock changes. If it does, commit the regenerated lock as part of the PR.

### Pitfall: cargo-deny duplicate-version warnings

When a bump pulls in a transitively-newer version of a shared crate (e.g., `hashbrown`, `cpufeatures`), cargo-deny's `multiple-versions = "warn"` fires. It's a warning, not an error, but accumulates. If a dep upgrade is the difference between one and two versions of a shared crate, mention it in the CHANGELOG so future debugging has a paper trail.

## CHANGELOG conventions for dep bumps

(Reinforces what `changelog` skill says about runtime cargo dep changes.)

| Scenario | CHANGELOG action |
|---|---|
| Patch bump, runtime dep, no source changes | One-line `### Changed` entry: "Bumped X 1.0.A → 1.0.B (patch)." |
| Minor bump, runtime dep, no source changes | One-line `### Changed` entry: "Bumped X A.B → A.B+1." Add a note if any user-visible behavior shift is expected. |
| Major bump, runtime dep, source migration required | Multi-line `### Changed` entry naming the API change AND the migration. Include the invariant preserved (e.g., "hash output unchanged"). |
| Bump alongside a feature PR | No separate entry — the feature's `### Added` line covers the dep arrival. |
| Dev-only bump (`[dev-dependencies]`) | No entry. |
| New dep added alongside a feature | No separate entry — feature entry covers it. |
| New dep added without a feature change | One-line `### Changed`: "Now depends on `X` for Y." |
| License added to `deny.toml` allow-list | No entry (CI tooling config, not binary). |

## Quick reference

| Situation | Action |
|---|---|
| Dependabot PR opened, CI green, runtime patch bump | Triage → local verify → CHANGELOG one-liner → merge (shape C) |
| Dependabot PR opened, CI green, runtime minor/major bump | Triage → local verify → if clean, same as above. If breaking, jump to shape selection. |
| Dependabot PR opened, CI failing | Read the error → classify → choose A/B/C → migrate → CHANGELOG → merge |
| Dependabot PR opened, dev-dep only | Triage → local verify → no CHANGELOG entry → merge |
| Sibling dep PRs land in sequence | Second PR rebases on updated main; resolve CHANGELOG conflict by stacking bullets |
| Reopen-after-close fails on API | Push to same branch, open fresh PR; dependabot infers from the eventual merge |
| Local cargo update (not from dependabot) | Same flow as a dependabot PR — verify locally, add CHANGELOG, commit + push as a regular PR |

## Related

- `changelog` skill — what counts as user-visible; CHANGELOG entry conventions
- `feedback-keep-deps-fresh` memory — the durable preference behind this skill
- `CHANGELOG.md` — the file itself
- `deny.toml` — license allow-list (only edit when adding a new permissive license a new dep ships under)
- `.github/dependabot.yml` — dependabot config (which deps it watches, on what cadence)
