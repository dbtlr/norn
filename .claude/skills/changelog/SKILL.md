---
name: changelog
description: Hard-and-fast rule and conventions for maintaining CHANGELOG.md in norn. Every user-visible change lands in `## [Unreleased]` BEFORE the change ships to main; promoted to a versioned section when a release is cut. Use when staging a commit, opening a PR, squash-merging to main, or cutting a release.
---

# CHANGELOG discipline for norn

## The rule (non-negotiable)

**Every user-visible change must appear in `CHANGELOG.md` under `## [Unreleased]` BEFORE the change lands on `main`.** No exceptions. A commit that adds/changes/removes user-visible behavior without a CHANGELOG entry is incomplete — go back and add the entry before merging.

This applies whether the change ships via:
- A direct commit to main
- A PR merge
- A squash-merge from a feature branch
- A cherry-pick from another branch

The rule is enforced at *change time*, not at *release time*. By the time a release is cut, `## [Unreleased]` should already be complete — promoting it to a versioned section is a rename, not an authoring pass.

## What counts as "user-visible"

**Guiding principle:** *Does this change land in the compiled binary the user runs?* If yes, it belongs in the CHANGELOG. If no — it lives only in the repo (CI config, contributor docs, dev-only tooling) — it does not.

This is a binary-effect test, not a "did the user request it" test. A runtime cargo dependency bump that ships in the linked binary is user-visible even though the user didn't ask for it; a `deny.toml` tweak that only changes what cargo-deny accepts in CI is not, because the user's binary is bit-for-bit identical either way.

**Requires a CHANGELOG entry:**

- New commands, subcommands, flags, options, or config keys
- Changed command behavior, default values, or output format
- Removed or renamed surface (binary names, command names, flags, config keys)
- New error variants or error messages users will see
- Performance characteristics meaningful enough to mention (e.g., "norn validate now under 100ms via cache")
- Plan/cache/index JSON schema changes (especially version bumps)
- Breaking changes — these get loud treatment, see below
- **Runtime cargo dependency *bumps* that land in the compiled binary** — version changes to existing `[dependencies]` entries (`Cargo.toml` or transitive `Cargo.lock`), feature-flag changes that alter what's linked in. Even when the change is "transparent" from a feature perspective, the bytes the user runs are different and there is no other paper trail in the CHANGELOG. *Adding* a new dep is different — it almost always rides along with a feature/change that already has its own CHANGELOG entry, and that entry implicitly covers the dep. Only call out the dep separately when it ships without a corresponding feature change.
- New dependencies that affect installation (e.g., a new C library, a different TLS backend that pulls in system libraries)
- File-location changes (cache path, log path, where norn writes user data)
- New permission requirements (e.g., file mode changes)
- Documentation contract changes (changing what an agent is told to do)

**Does NOT require a CHANGELOG entry:**

- Internal refactors with no observable user/agent difference
- Test additions or test infrastructure
- **Dev-dependency-only changes** (`[dev-dependencies]`, build-only deps that don't link into the binary) — these affect the test runner / build process, not the shipped artifact
- **CI/CD configuration changes** — workflow files, GitHub Actions versions, runner setup, `cargo-dist` workflow regeneration
- **Linter / formatter / security-scanner config changes that don't affect the binary** — `deny.toml`, `rustfmt.toml`, `clippy.toml`, `.editorconfig`. These shape what the toolchain accepts at build time but produce the same compiled output.
- Code style fixes (rustfmt, clippy lint compliance)
- Repo-only documentation changes that don't change a documented user/agent contract (README typo fixes, contributor-facing CONTRIBUTING.md edits, internal architecture notes)
- Comments and inline documentation in source files

**The compiled-binary test in practice:**

| Change | Lands in binary? | CHANGELOG? |
|---|---|---|
| Add `ureq = "2.10"` to `[dependencies]` **as part of a new feature that needs HTTP** | Yes (linked) | No separate entry — covered by the feature's own `### Added` line |
| Add `ureq = "2.10"` to `[dependencies]` with no corresponding feature change (e.g., preparing infrastructure for a later release) | Yes (linked) | Yes — one line under `### Changed` |
| Bump `serde` from 1.0.150 → 1.0.200 (no API change, just a transitive bytes change) | Yes (different bytes) | Yes — one line under `### Changed` |
| Drop a previously-used runtime dep (e.g., replace `reqwest` with `ureq`) | Yes (different bytes) | Yes — one line under `### Changed`; mention what's replaced |
| Add `mockito` to `[dev-dependencies]` | No (test-only) | No |
| Add `CDLA-Permissive-2.0` to `deny.toml` allow-list | No (lints CI, not the binary) | No |
| Bump `actions/checkout` from v5 → v6 in `.github/workflows/ci.yml` | No (CI tool, not the binary) | No |
| `cargo fmt` sweep across the repo | Yes technically (same semantics, identical compiled output) | No — same observable bytes are emitted by the compiler |
| Bump `cargo-dist` and regen `release.yml` | Sometimes — if installer scripts change, that's user-visible; if only the workflow YAML changes, no | Judgment call; lean toward "yes" when the installer scripts change |

When in doubt: add an entry. Operators reading the CHANGELOG would rather see a sentence they can skip than miss a real change.

## Section structure

The file follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Top of file:

```markdown
# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
once it ships v1.0. Pre-1.0 versions may include breaking changes in minor releases.

## [Unreleased]

Entries here have landed on `main` but have not yet been cut into a tagged release. When a release is cut, this section is promoted to `## v0.X.0 - YYYY-MM-DD` and a fresh `## [Unreleased]` header is added above it.

### Breaking changes
### Added
### Changed
### Fixed
### Known limitations

## v0.X.0 - YYYY-MM-DD
...
```

Subsection order is fixed (Breaking changes → Added → Changed → Fixed → Known limitations). Omit a subsection entirely if it would be empty. Optional `### Notes` subsection at the end for follow-up pointers (e.g., "telemetry follow-up tracked separately").

## Subsection guidelines

### Breaking changes

Loud, explicit, named. Every breaking change names:

1. **What broke** — the specific surface, version threshold, error users will see.
2. **The migration path** — what the user has to do, even if "no migration shim" (which is the pre-1.0 default).
3. **The scope of the blast radius** — what else gets affected.

Example (good):
> **Repair plan JSON schema bumps from v3 to v4.** `norn repair apply` rejects v3 plans with `unsupported repair plan schema version: expected 4, got 3`. No migration shim. Regenerate any persisted plans with `norn repair plan` against v0.28.0+.

Example (bad — too vague):
> Breaking change: schema bumped.

### Added

Lead with what the user/agent sees, not the internal implementation. Include the command, flag, or config key so it's grep-able.

Example (good):
> `add_frontmatter` repair action — inserts a missing frontmatter field with a literal value. Refuses if the field already exists (use `set_frontmatter` for replacement). Same minimal-edit YAML preservation as set/remove.

Example (bad — implementer-narrative):
> Added a new RepairAction enum variant in vault-standards/src/config.rs.

### Changed

Behavior or default that already existed but now works differently. For changes that don't break callers, this is the right section (vs Breaking changes).

### Fixed

Bug fixes shipping to main. Name the symptom users observed, not the internal cause.

Example (good):
> Markdown link rewriting on `move_document` apply now correctly handles bare-URL `raw` form. Previously, `norn repair apply` would silently no-op on Markdown link rewrites; only wikilinks were rewritten.

### Known limitations

V1 trade-offs that are intentional but worth documenting. Each entry names the symptom, the workaround, and what triggers the eventual fix.

Example:
> When a backlinking file contains multiple identical link occurrences pointing at a moved file, `repair apply` rewrites only the first occurrence. Subsequent occurrences flag as unresolved on the next `norn validate`. To be addressed in a follow-up by adopting byte-span-precise edits.

## Cutting a release

1. **Confirm `## [Unreleased]` is complete.** Walk the commit log since the previous release; every user-visible commit should be represented.
2. **Rename `## [Unreleased]` to `## v0.X.0 - YYYY-MM-DD`.** Use today's date in `YYYY-MM-DD` format.
3. **Add a fresh `## [Unreleased]` above the newly-versioned section.** Include the standard intro paragraph and empty subsection headers (or leave them out until the first entry).
4. **Optional but recommended:** add a brief release-direction paragraph after the version header, summarizing the release's theme.
5. **Bump the workspace version in `Cargo.toml`** if applicable.
6. **Commit** with message like `Cut v0.X.0 release` or fold into the work commit that triggered the release.

## Three-layer durability

The CHANGELOG is one of three layers; the others kick in when you need to recover detail the CHANGELOG doesn't preserve:

1. **`CHANGELOG.md` `## [Unreleased]`** — human-curated release notes. The primary surface for operators.
2. **Squash commit bodies** — when squash-merging a feature branch, include the per-task narrative inline with original commit SHAs. `git log -1 <squash-sha>` then recovers everything that landed. Especially valuable when the feature branch gets deleted post-merge.
3. **Atlas vault workspace + session logs** — durable design knowledge in `Workspaces/norn/` (`decisions/`, `notes/`) plus the Saga session logs in `artifacts/session-logs/`. The deepest archive, capturing rationale + alternatives considered + risks. (Specs/plans are transient — reviewed in `artifacts/scratch/`, deleted on merge.)

Don't duplicate effort across layers. Each answers a different question:
- "What shipped in this release?" → CHANGELOG
- "What's the history of this code?" → `git log`
- "Why was it built this way?" → atlas vault

## Anti-patterns

**Forgetting to update CHANGELOG until release time.** The rule is at-change-time. Catching up at release-time means walking `git log` from memory, missing things, writing vague summaries. Adding the entry while the change is fresh in your head is dramatically cheaper.

**"Various improvements" / "Bug fixes".** Name them. If the work was worth shipping, it's worth describing.

**Pasting commit messages verbatim.** Commit messages talk to other engineers in implementer-narrative ("refactor parse loop to avoid clone"); CHANGELOG entries talk to operators and agents in user-narrative ("`norn validate` is now ~30% faster on large vaults"). Translate.

**Leaving `## [Unreleased]` empty between releases.** If the file's last entry is a versioned release and there's no `## [Unreleased]` above it, the next change author has to remember to add the header. Always keep an `## [Unreleased]` heading at the top, even if empty.

**Hiding breaking changes under "Changed".** If a caller has to update their code, it's breaking. Loud treatment under its own `### Breaking changes` heading.

**Promoting `## [Unreleased]` partially.** When cutting a release, promote the whole section. Don't leave some entries unreleased while versioning others.

## Worked example: this session's pattern

The SQLite cache work (v1, shipped 2026-05-19, commit `be02575`) was added to `## [Unreleased]` rather than getting its own version bump because the plan is to bundle v1 with the future v2 SQL-direct release. The pattern:

1. The cache work landed on `main` via squash-merge.
2. The squash commit body contains the full 12-commit narrative with original SHAs.
3. `CHANGELOG.md` gained a fresh `## [Unreleased]` section with the cache v1 entry — describing what users get, what changed in the schema, what's still to come in v2.
4. When v2 lands, it appends to the same `## [Unreleased]`. The eventual release cut renames `## [Unreleased]` to (e.g.) `## v0.29.0 - YYYY-MM-DD`.

This is the standard mechanism for "group multiple tasks into a release without losing per-feature change history." Use it.

## Quick reference

| Situation | Action |
|---|---|
| Adding a new flag, command, or behavior | Add bullet under `### Added` in `## [Unreleased]` |
| Changing default or existing behavior (non-breaking) | Add bullet under `### Changed` |
| Removing or renaming surface | Add bullet under `### Breaking changes` with migration path |
| Bumping schema version (plan, cache, etc.) | Add bullet under `### Breaking changes` with rejection-message text |
| Fixing a bug operators have hit | Add bullet under `### Fixed` describing the user-visible symptom |
| Adding a runtime cargo dep AS PART OF a new feature | No separate entry — the feature's `### Added` line covers it |
| Adding/removing a runtime cargo dep WITHOUT a corresponding feature change | One-liner under `### Changed` ("now depends on `ureq` for HTTP"; "dropped `reqwest`") |
| Bumping an existing runtime cargo dep version (different bytes in the binary) | One-liner under `### Changed` — keep it terse unless behavior is affected |
| Adding a dev-dependency or CI tool config (no effect on binary) | No CHANGELOG entry needed |
| Tweaking `deny.toml`, `rustfmt.toml`, `.github/workflows/*` | No CHANGELOG entry needed |
| Internal refactor with no observable change | No CHANGELOG entry needed |
| About to squash-merge a feature branch | Verify the feature's entry is in `## [Unreleased]`; rich squash body covers history |
| Cutting a tagged release | Promote `## [Unreleased]` → `## v0.X.0 - YYYY-MM-DD`; add fresh `## [Unreleased]` above |

## Related

- `CHANGELOG.md` — the file itself
- v0.28.0 release entry — reference example of full Breaking changes + Added + Changed + Known limitations layout
- `## [Unreleased]` cache v1 entry — reference example of the unreleased-during-dogfood pattern
- Atlas vault `agent-artifacts/` for deeper design archive
