---
title: "0008 — Frontmatter mutation is minimal-edit byte-splicing, not reserialize"
description: "Architectural decision that norn mutates frontmatter by splicing individual field byte-ranges rather than parse-and-reemit, owning scalar quoting via real-YAML round-trip verification — to preserve comments, quote style, key order, and minimal diffs."
---

# 0008 — Frontmatter mutation is minimal-edit byte-splicing, not reserialize

**Decision:** norn's frontmatter mutations edit the source file by **splicing individual field byte-ranges** — located by a scanner (`top_level_property_spans`) and replaced via `String::replace_range` in `apply_file_changes` (`src/standards/apply.rs:340`) — rather than parsing the frontmatter to a structured value and re-emitting the whole block. Scalar **quoting is owned by norn** (`serialize_value_preserving_style`, `src/frontmatter/quote.rs`) and made correct by **round-trip verification against a real YAML parser** (emit at the preferred style, reparse via `serde_yaml`, escalate plain → single → double until byte-identical) — *not* by handing emission to `serde_yaml`'s normalizing emitter.

## Context

Minimal-edit is a deliberate, test-enforced contract. Splicing one field's value range leaves every other byte untouched, which preserves:

- **same-line comments** (`status: someday  # legacy` → `status: done  # legacy`; `apply.rs:1150`),
- **quote style** (double stays double, single stays single; `quote.rs:437-590`),
- **blank lines and exact body bytes** (`apply.rs:1243`),
- **key order** (a side effect of never rebuilding the mapping).

This serves the inspectable-vault / minimal-diff northstar: a mutation produces the smallest possible git diff, and the file stays human-legible and Obsidian-native. The three layers (source files are the tables, git is the write-ahead log) only pay off if a one-field edit reads as a one-field diff.

The cost of minimal-edit is that norn owns two things a normalizing emitter would hand it for free: **scalar quoting correctness** and **field byte-range location**. Both are accepted and managed:

- *Quoting* is solved by round-trip verification against the real YAML standard, not a hand-maintained denylist — the invariant catches any hazard (trailing colon, embedded newline, numeric-looking string, and ones not yet seen).
- *Range location* is a hand-rolled line scanner that is fragile against YAML layout variety (no-indent block sequences broke it). Its robustness is tracked separately, with the option to back range-location with a marker-aware parser *while keeping the minimal splice*.

**Alternatives rejected:**

- *Full reserialize through `serde_yaml`'s emitter* — simplest to implement. Rejected: it scrambles key order (the parsed value is a `serde_json` `Map` over `BTreeMap`, i.e. alphabetical — `parse.rs:30`), strips comments, normalizes quote style, and churns a large diff on every `set`. That breaks the inspectable-vault contract that is the whole reason data lives in flat Markdown rather than a database.
- *Order-preserving reserialize* (`IndexMap` + carry comments/quotes/blank-lines forward) — recovers most preservation properties, but at that point you have reimplemented a format-preserving emitter with more moving parts than span-splicing, and it still silently normalizes anything you forgot to carry. Rejected: minimal-edit gives the same guarantees more simply.

## Consequences

- **norn owns scalar quoting**; correctness comes from round-trip verification against `serde_yaml`, not a denylist. New hazards are caught by the round-trip check, never by adding denylist entries.
- **norn owns field byte-range location** via `top_level_property_spans`; that scanner's robustness against YAML layout variety is an ongoing concern (the no-indent case is patched; the class is tracked and may graduate to parser-located spans).
- **The mutation funnel is shared:** `set`, `migrate`, repair-plan apply, `vault.set`, and `vault.apply_plan` all pass through `apply_file_changes`. `norn new` is the one exception — a whole-document create via `serialize_new_document`, which legitimately starts from an empty map.
- This decision flips only if the inspectability / minimal-diff value is abandoned — unlikely post-1.0.
**Amendment (2026-07-06) — post-image verification gate.** Minimal-edit correctness is now backed by a whole-mapping oracle beyond the per-scalar round-trip quoting check: **no frontmatter mutation is written unless the mutated block re-parses to exactly the intended mapping.** This covers every write path — op-based `set`/`remove`/`add`, on-document `rewrite_link`, move/delete backlink cascades, and the `new` create path (`verify_created_document`). A mutation whose post-image would not reconstitute is refused (exit 2; cascades skip-with-report, code `would_corrupt_frontmatter`) rather than written. This is what makes minimal-edit safe against the scanner's fragile span location: the splice is *proposed* by the line scanner but *vetoed* by the value oracle (scalar) and the post-image mapping oracle (whole block). It is the natural acceptance criterion for the purpose-built parser — 'the gate never fires.' Related contract change: empty list fields serialize as `field: []` (read back `[]`, not null).
