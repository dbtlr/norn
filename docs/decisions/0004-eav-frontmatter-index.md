---
title: "0004 — Frontmatter indexing is an EAV narrow table with sentinel completion"
description: "Architectural decision specifying norn's frontmatter index as a derived entity-attribute-value narrow table with typed values and sentinel rows for total field coverage, enabling indexed query routing without changing query semantics."
---

# 0004 — Frontmatter indexing is an EAV narrow table with sentinel completion

**Decision:** norn's frontmatter index (Wave 2 of *the consumer capability-asks note (internal design doc)*) is a **derived EAV narrow table** — `document_fields(path, key, value)` — maintained alongside the untouched `documents.frontmatter_json` retrieval store. Arrays shred into repeated `(path, key)` rows, one per element. The `value` column has **no type affinity and stores typed SQL values** (INTEGER/REAL/TEXT via the same canonicalization the query layer applies today, wikilink brackets collapsed at write time). Every doc gets **total coverage over the declared index set**: absent-or-null fields write a **BLOB `x'00'` sentinel row** — type-disjoint from every legal value by construction, since canonicalization can only produce NULL/INTEGER/REAL/TEXT. Empty arrays write a NULL-value presence row. Query routing is **all-or-nothing**: a filter whose fields are all indexed compiles to indexed subquery plans; any unindexed filtered field falls back to today's `json_extract` scan, with a warn above 1,000 scanned docs.

## Context

The cache stored all frontmatter as one `frontmatter_json` TEXT column with no field indexes, so every filtered `find`/`count` was O(vault) — a full JSON scan, with array predicates paying a correlated `json_each` subquery per row. Large knowledge bases are the adoption target; linear query degradation is fatal there. ADR [0003](./0003-relational-engine-over-flat-files.md) names the engine form this serves: the narrow table is a pillar of the SQLite-cache query layer, derived data only.

**Alternatives rejected:**

- *Per-field expression indexes / generated columns* — schema churns on every hot-set change (ALTER/reindex), and array fields still need child tables; the narrow table is schema-stable (hot-set change = delete + re-insert rows, no DDL).
- *Shred-everything EAV* — indexing free text bloats the index with paragraph-sized values and defeats the planner; the index carries only declared, bounded fields.
- *Canonical-text `value` column* — breaks numeric equality semantics (`rank: 5.0` stored as `"5.0"` no longer matches `--eq rank:5`). SQLite's dynamic typing makes typed storage free and preserves cross-type numeric comparison exactly as the scan path behaves.
- *Hybrid routing (indexed predicates drive, cold fields as residual filters)* — deferred, not rejected. All-or-nothing is the simpler v1 and the index-content policy (bounded types only) makes cold-field fallback the designed behavior, not a failure mode.

**Indexing policy:** a global `autoIndex` (default **true**) indexes every field carrying a bounded type in any rule — `allowed_values` enums, `date`/`datetime`, `wikilink`/`wikilink_or_list`, and bounded strings. Per-field `indexed: true/false` lives next to the type declaration and overrides both directions; cross-rule conflicts resolve any-`true`-wins (the index is vault-global). `autoIndex: false` flips to opt-in for huge vaults that want a manual hot set. The type vocabulary gains **`string` (bounded by default: 64 chars, `max_length` expandable to a hard 256 ceiling)** and **`text` (unbounded, never auto-indexed)**; `list_of_strings` becomes a list of bounded `string`s and therefore auto-indexable. A `tag` type (string-shaped, link-behaved, graph-driving) is reserved for the future — this wave's bounded strings deliberately do not claim the name.

**Sentinel rationale:** total coverage makes absence a first-class indexed fact — `--missing` compiles to a driving `SEARCH (key, value=sentinel)` instead of a per-doc anti-join, and negation becomes one uniform probe (`NOT EXISTS(key=? AND (value IN (…) OR value=sentinel))`) correct for scalars, arrays, and missing fields alike. The sentinel is **mechanism, never surface**: no query syntax addresses it, and it is filtered from every user-facing output.

A 50k-doc / 365k-row planner spike (2026-07-02) validated every shape via `EXPLAIN QUERY PLAN`: R3 positive-drive + negation-probe 3.2ms (scan baseline 15.8ms), R5 reverse-dependency sub-ms, `--missing` as driving search, B1 prefix as a range `SEARCH` unaffected by BLOB sentinels in the index, cross-type numeric equality preserved, and 3-way intersection planned with bloom filters — no `SCAN` anywhere.

## Consequences

- **Semantics parity is the hard invariant.** A derived index must never change results: every query shape returns identical output whether a field is indexed or not, pinned by an indexed-vs-scan parity property test. The write-time canonicalizer and the query-side conversion must share code, not mirror it.
- **The narrow table serves WHERE only.** Sort stays `ORDER BY json_extract` on the filtered set; `count` shares the predicate router for filters but groups via `json_extract`. Filterless `count --by` remains O(vault) by nature — measured, and EAV-served grouping becomes a follow-up only if it misses budget.
- **Maintenance rides existing machinery.** Per-doc updates extend the blake3-gated writer transaction (delete + re-insert that path's rows — the table can never drift from `frontmatter_json`). A resolved-index-set hash in `meta` triggers a silent cache-side re-shred on config change (no vault re-parse). Shipping is a `SCHEMA_VERSION` bump. `ANALYZE` runs after rebuild and re-shred so the planner has real cardinalities.
- **Acceptance is plan-shape + timing curve.** EXPLAIN guard tests (R3, R5, `--missing`, B1 prefix, adversarial non-selective, sentinel invisibility) plus synthetic 1k/10k/50k timings asserting indexed queries stay flat, plus the standing real-vault dogfood budget (<50ms warm).
- **Numeric range/sort is unlocked, not deferred.** Typed values mean numeric operators need no future schema change — only grammar, if ever wanted.
- **Deferred with clean seams:** hybrid routing for mixed hot/cold filters; index-served ORDER BY; EAV-served grouping; the `tag` type and pattern constraints (rules-expressiveness brainstorm).
