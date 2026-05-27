# Contributing to norn

Thanks for considering a contribution. `norn` is a deterministic Markdown vault graph, link, and validation tool. Contributions of all sizes are welcome — bug reports, doc fixes, test additions, and feature work.

## Getting started

Local development setup, the `mise` and `just` workflow, and the MSRV policy are documented in [`docs/development.md`](docs/development.md). Read it first; the short version is:

```bash
mise install
just verify
```

## Filing issues

- Bug reports: [open a new bug issue](https://github.com/dbtlr/norn/issues/new?template=bug.md). Include `norn --version`, your platform, a minimal reproducer (a few files in a temp directory is ideal), the command you ran, and the output you got versus what you expected.
- Feature requests: [open a new feature issue](https://github.com/dbtlr/norn/issues/new?template=feature.md). Describe the user problem, then the proposed shape of the solution.

## Pull requests

- Keep PRs small and focused — one logical change per PR. Large refactors are easier to review when split into reviewable slices.
- Run `just verify` locally before pushing. CI runs the same checks.
- If the change affects CLI behavior, output format, or configuration, add a CHANGELOG entry under the appropriate `Added`, `Changed`, `Removed`, or `Fixed` heading. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
- If the change adds or modifies a doc page, make sure the page has `title` and `description` frontmatter so the repo's own `norn validate` check stays green.

## Commit messages

Conventional, plain-English commit messages. The first line is a short imperative summary (under ~72 chars). Body paragraphs explain the why, not the what. Examples in `git log` are the best style reference.

## Code of conduct

Be respectful. Assume good faith. Disagreements about design are normal; personal attacks are not. Maintainers reserve the right to close threads or block participants who make the project unwelcoming to others.

## Security

Security issues should not be filed as public issues. See [`SECURITY.md`](SECURITY.md) for the disclosure process.
