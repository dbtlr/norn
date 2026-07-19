---
title: "0019 — code blocks are opaque to all targeted parsing"
description: "Architectural decision that fenced code blocks and inline code spans are treated as a foreign document — no semantic body parser (wikilinks, Markdown links, headings, block-ids, tags, any future extraction) may match inside them."
---

# 0019 — code blocks are opaque to all targeted parsing

Content inside a fenced code block (``` ``` ```/`~~~`) or an inline code span (`` `…` ``) is **opaque** to every targeted body parser. **Decision:** a code region is a different document — no semantic extraction (wikilink syntax, Markdown links, headings, block-ids, tags, and any parser added later) may recognize a token inside it. What a human reads as a literal code sample, norn must read as literal text, never as vault structure.

One Obsidian-aligned nuance is preserved: a `^block-id` on the line *after* a closing fence references the code block itself and remains a valid anchor. The exclusion covers what is *inside* the fences, not the anchor line that trails them.

## Context

The rule was already partially in force but not stated: `parse_wikilinks` excludes inline/fenced code ranges (a `[[…]]` in a code sample is literal, not a link), and the `pulldown-cmark`-based heading and Markdown-link parsers skip code events for free — a `#` or `[text](url)` inside a fence never emits a `Tag::Heading` / `Tag::Link`. The gap was `parse_block_ids`, a line-oriented regex scan with no code awareness: a `^id` inside a code fence was registered as a real anchor, so a `[[Note#^id]]` reference could resolve to a block that exists only as sample text. That is drift the northstar forbids — the vault's anchor universe must mean what a reader would agree it means.

Stating the rule once, at the crate level, means future parser authors inherit it rather than rediscovering it per feature.

## Consequences

- `parse_block_ids` gains the same fenced-plus-inline code-range exclusion `parse_wikilinks` uses. A `^id` inside a fence or an inline span is no longer an anchor; a `^id` on the line after a fence still is.
- The rule is stated in the `norn-frontmatter` crate-level docs so every current and future body parser is bound by it.
- The anchor universe shrinks relative to the pinned oracle (0.48.1), which registers code-fenced block-ids: a `[[Note#^id]]` pointing at a code-fenced `^id` resolves under the oracle but is unresolved under the rewrite. This is a decided, ledgered divergence (see `docs/parity-ledger.toml`).
- Any body parser added later (tags, embeds, transclusions, further anchor forms) must honor code opacity or explicitly justify the exception against this decision.
