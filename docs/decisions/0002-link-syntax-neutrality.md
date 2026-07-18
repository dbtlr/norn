---
title: "0002 — norn is link-syntax-neutral; Obsidian-isms are opt-in"
description: "Architectural decision establishing that norn treats relative Markdown links as the first-class default link syntax while keeping Obsidian wikilinks fully supported as an opt-in, non-default form."
---

# 0002 — norn is link-syntax-neutral; Obsidian-isms are opt-in

norn supports both relative Markdown links (`[label](../notes/foo.md)`) and Obsidian wikilinks (`[[target]]`). **Decision:** relative Markdown links are the first-class, default idiom in all documentation, examples, and future link features; wikilinks stay fully supported but are positioned as an opt-in Obsidian-compatible form, never the default. norn's promise is the power of Obsidian without Obsidian — a user who dislikes Obsidian's Markdown extensions must never feel tied to them.

## Context

As of 2026-06-01 the tool is already mostly neutral: parsing, resolution, `find --links-to` / `--unresolved-links`, `get` link facets, `validate` link findings, and `move` / `delete` cascade rewrites all cover both syntaxes in document bodies (relative Markdown links get a recomputed relative path on move); frontmatter fields are the tracked exception below. The lean toward wikilinks was cultural in the prose, not a tool limitation. Two real asymmetries remain (tracked as tasks, below).

## Consequences

- Per-command docs and the SKILL lead every link example with relative Markdown links; wikilinks appear as the opt-in alternative.
- The glossary `Vault` definition reads "frontmatter and links," naming wikilinks as a supported Obsidian-compatible form rather than the defining trait.
- Future link surfaces must support both syntaxes or explicitly justify the asymmetry against this decision.
- This governs *positioning and parity*, not the link style norn *writes*. Auto-wrapping a `wikilink`-typed field on `set` stays config-driven and opt-in — neutrality does not force a write style either way.

## Known asymmetries (tracked)

- *the generic-relative-link-rewrite note (internal design doc)* — `rewrite-wikilink` is wikilink-only; no standalone retarget for a relative Markdown link.
- *the frontmatter-Markdown-link-parsing note (internal design doc)* — frontmatter parsing recognizes only wikilinks, not Markdown links.
