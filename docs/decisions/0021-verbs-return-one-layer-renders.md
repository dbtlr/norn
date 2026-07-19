---
title: "0021 — verbs return values, one layer renders"
description: "Architectural decision that CLI command modules resolve a report and return a value, and a single display-layer emit call performs all rendering — format resolution, palette, records composition, stderr annotations, and exit code — so stdout stays payload-only by construction."
---

# 0021 — verbs return values, one layer renders

A `norn` command module resolves its report and **returns a value**; a single
render call in the display layer turns that value into bytes. **Decision:** no
command module writes stdout, formats a `--format` choice, resolves a palette, or
decides tty-versus-pipe defaulting. Each verb's `run` returns
`Result<Output, Diagnostic>` — the report (plus the presentation parameters a
renderer needs) on success, a structured diagnostic on failure — and the one
`emit` call in the dispatch loop resolves the format, resolves the palette once,
composes records through the styling primitives, routes report annotations to
stderr, and derives the process exit code. Rendering is correct by construction
because the only code that can reach the payload stream is the layer itself.

The rule exists because presentation concerns — color, isatty defaulting, column
projection, the trailing-newline discipline, the stdout/stderr split — were
previously re-decided per verb, and per-verb copies drift. A trustworthy vault
tool cannot afford an output surface where each command is its own precedent.

## Context

The read verbs each hand-rolled their own format enum and render functions, each
re-resolving the palette, re-checking isatty, and re-implementing the `--col`
projection ladder (the split into structural facets and frontmatter fields, then
the per-facet folding). Two verbs carried near-identical copies of that ladder;
the registry `list` verb carried a second, unrelated `Human | Json` format
scaffolding. Every copy is a place the stdout/stderr contract or the byte format
can quietly diverge from its sibling.

## The invariants

- **No stdout in command modules.** A verb returns an `Output`; a stderr-only
  conversation handle carries report annotations. A guard test rejects the
  `print!` / `println!` / `eprint!` / `eprintln!` macros anywhere under the
  command modules, so the contract is enforced structurally, not by review.
- **One semantic format vocabulary.** A single `Format` enum
  (`records` / `paths` / `json` / `jsonl` / `markdown`) is the display layer's
  rendering vocabulary. Each verb's per-command value-enum stays as the truthful
  `--help` declaration of the subset it accepts and maps into `Format`, so a verb
  can never resolve to a format it does not support.
- **The layer resolves the palette.** Color resolution happens once, inside the
  render path, never in a command module. The record primitives take styling
  internally, so an unstyled record block is unrepresentable.
- **The layer owns isatty defaulting.** Each output kind declares its
  `{ tty, piped }` default pair; when `--format` is absent the layer picks by
  whether stdout is a terminal. Each verb's existing default is preserved exactly
  — only the query verb differs between terminal and pipe.
- **One projection implementation.** The `--col` split and facet-folding logic
  exists once and serves every format and every verb; the only per-verb
  difference (which columns the no-`--col` default emits) is a parameter, not a
  fork.
- **stdout is payload-only, structurally.** Diagnostics keep the single
  prefixed-headline-plus-hints path on stderr; report annotations
  (`note:` / `warning:` lines) travel a distinct stderr channel; the payload
  stream stays clean in every format so a consumer can always `2>/dev/null`.

## Consequences

- The dispatch loop is the one place a report becomes bytes: format, palette,
  width, annotations, and exit code are resolved there and nowhere else. A new
  verb supplies a report and a default pair, not another renderer.
- Piped output is unchanged across the refactor — the byte format each verb
  emits is reproduced exactly, verified against the pinned-oracle parity harness.
  One deliberate exception rides along: the registry verbs now fold their
  configuration errors through the same diagnostic constructor the read verbs
  use, so an error like "unknown vault name" gains its recovery hint on every
  surface instead of only some.
- Future presentation primitives (a severity tally, a change-line for mutation
  reports) land as additions to the one styling vocabulary, inheriting the
  stdout/stderr contract rather than re-deciding it per verb.
