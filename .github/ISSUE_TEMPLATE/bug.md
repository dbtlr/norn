---
name: Bug report
about: Report a bug or unexpected behavior in vault-cli
title: "Bug: "
labels: ["bug"]
assignees: []
---

## What happened?

A clear description of the problem.

## What did you expect?

A clear description of the expected behavior.

## Reproducer

The minimum set of files and commands that reproduce the issue. A few Markdown files in a temp directory, plus the exact `vault` command line, is usually enough.

```bash
# Setup
mkdir /tmp/repro && cd /tmp/repro
# ... commands to create the failing state ...

# Command that triggers the bug
vault -C . validate --summary --format json
```

## Output

The actual output (paste verbatim, redact anything sensitive). For JSON or JSONL output, pretty-print it if helpful.

```text
<paste output here>
```

## Environment

- `vault --version`:
- OS and version (e.g. macOS 14.5, Ubuntu 24.04):
- Install method (shell installer / `cargo install` / source build):
- Rust version (`rustc --version`) if built from source:

## Additional context

Logs, stack traces, related issues, hypotheses — anything that helps narrow it down.
