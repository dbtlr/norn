---
title: Releases and versioning
description: Release workflow, semver and Keep a Changelog conventions, recommended branch protection, and the cargo-dist recipe.
---

# Releases and versioning

`vault-cli` is pre-1.0. Minor releases may include breaking changes (CLI flags, config keys, JSON contracts). Breaking changes are called out in [CHANGELOG.md](../CHANGELOG.md) with migration notes.

## Versioning

Use semver-style tags: `vMAJOR.MINOR.PATCH`.

- **Major.** Reserved for v1.0 and beyond.
- **Minor.** New behavior, new commands, breaking changes (pre-1.0).
- **Patch.** Bug fixes, doc-only changes, dependency bumps.

Once v1.0 ships, the project will commit to semver guarantees in the usual way. Until then, read the CHANGELOG before upgrading.

## Changelog format

The [CHANGELOG.md](../CHANGELOG.md) follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) conventions. Each release entry uses these sections in order:

- `### Breaking changes`
- `### Added`
- `### Changed`
- `### Removed`
- `### Fixed`
- `### Internal`

Unreleased changes accumulate under `## Unreleased`. At release time, the heading flips to `## vX.Y.Z - YYYY-MM-DD`.

Every versioned change updates the CHANGELOG in the same work slice. The PR template surfaces this requirement.

## Release workflow

For a release bump (from a clean working tree on `main`):

1. Update workspace version in `Cargo.toml`.
2. Run `cargo check` to refresh `Cargo.lock`.
3. Run `mise exec -- just verify`.
4. Update `CHANGELOG.md`: rename `## Unreleased` to `## vX.Y.Z - YYYY-MM-DD`.
5. Commit `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md`.
6. Tag: `git tag -a vX.Y.Z -m "vault-cli vX.Y.Z"`.
7. Push: `git push && git push --tags`.

The `just release <version>` recipe automates steps 1, 2, 5, and 6.

## cargo-dist release flow

Releases use [cargo-dist](https://github.com/axodotdev/cargo-dist) for binary builds, the hosted shell installer, and GitHub Releases artifacts. Configuration lives in `dist-workspace.toml` at the repo root.

Before tagging, validate the dist plan:

```bash
mise exec -- just dist-plan       # cargo dist plan
mise exec -- just dist-build-local # cargo dist build (local artifacts only)
```

`dist plan` asserts the configuration is sane. `dist build` confirms artifacts assemble locally. Neither publishes.

Tagging triggers `.github/workflows/release.yml`, which builds binaries for each target, packages them with completions and the man page, generates the shell installer, creates the GitHub Release, and uploads assets.

Initial release targets:

- `aarch64-apple-darwin`
- `x86_64-apple-darwin`
- `x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl`

Windows is deferred. Homebrew tap and npm wrapper are tracked as follow-up work.

## Recommended branch protection

For `main`:

- Require pull request reviews before merging.
- Require status checks to pass (`ci` workflow).
- Require branches to be up-to-date before merging.
- Restrict who can push directly (admins only).

For tags:

- Restrict tag creation to admins.
- Require the `release` workflow to pass before the release is marked latest.

## Recent schema breaks

### v0.28.0 schema break

The repair plan JSON schema bumps from v3 to v4 in v0.28.0. `vault repair apply` rejects v3 plans with `unsupported repair plan schema version: expected 4, got 3`. No migration shim. Regenerate any persisted plans with `vault repair plan` against v0.28.0+.

## Post-release verification

After tagging, verify the release on a clean machine:

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/dbtlr/vault-cli/releases/latest/download/vault-cli-installer.sh \
  | sh
vault --version
vault -C fixtures/basic validate --summary --format json
```

If the install or the smoke test fails, mark the release as a pre-release in GitHub and investigate before announcing.

## See also

- [Development](development.md) — local build and test workflow.
- [Installation](installation.md) — user-facing install paths.
