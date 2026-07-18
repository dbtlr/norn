---
title: "0003 — norn is a relational query/mutation engine over flat text files"
description: "Architectural decision framing norn as a relational query and mutation engine over flat, git-versioned text files, with source files as tables, a SQLite cache as the query engine and index, and git as the write-ahead log."
---

# 0003 — norn is a relational query/mutation engine over flat text files

**Decision:** norn's *form* is a relational engine whose tables are flat, human-readable text files — SQLite's query/index/transaction semantics, git's durability, Obsidian's inspectability. More precisely, a **document store + link graph + relational constraints, over flat files**. This is the crystallized *shape* of the existing northstar (a deterministic substrate for a consistent human+agent shared vault), not a replacement for it. The "shared vault / MCP / claude.ai-queries-my-vault" use case is now a **consequence** of being a real database over files, not the direction.

## Context

Speccing an external consumer's adoption of norn as its system of record (see *the consumer capability-asks note (internal design doc)*) forced the question of what norn *is*. "Interacts with a Markdown directory like Obsidian" is too weak to explain why the write surface, stronger relational rules, indexing, and a wider conditional query grammar all belong. The stronger frame: **you interact with a directory of Markdown the way you'd use SQLite — except the tables are flat files you can version-control and open in Obsidian.** norn already has this architecture; this ADR names it.

The three layers: **source files are the tables** (source of truth) · **the SQLite cache is the query engine and index** (derived, disposable) · **git is the write-ahead log** (undo). The market problem this solves — "I handed Obsidian to Claude and got a mess" — is a filesystem with no schema, constraints, or transactions; norn supplies exactly those, which is why *consistency is still the end* and this is its mechanism.

The mental model draws on a *family* of primitives to reason with, not clone: SQLite (query / index / transaction), Postgres (`jsonb`, typed constraints, FDW adapters, `pgvector`), Mongo (document records), SQL sub-queries (filter-by-another-query). norn is the flat-file, git-versioned, human-readable member of that family.

## Consequences

- **Growth law.** norn grows the way Postgres/SQLite grew: by absorbing capabilities as pillars — a `(type, index, operator)` triple, or an adapter — never by forking into a new paradigm. Scope test for any future ask: *relational pillar rendered for flat files, or a fork?* Pillars in; forks out.
- **Roadmap re-ranking.** Indexing (F1/F2), the wider query grammar (B1 string operators, F3 aggregation), the write surface (H1–H3 plan-composable edits/creates), and stronger relational rules (D1 any-of selector, D2 typed references, incremental-path/sequence rules) are the spine. MCP/off-host transport is downstream — a consequence, still a golden use case, no longer the heading.
- **Hard invariant.** Derived data — expression indexes, normalized relations, embeddings, blob metadata — lives *only* in the cache. Flat files stay the human-readable source of truth; git stays the log. This is the line that keeps "revert with git, open in Obsidian" true as pillars accrue, and the reason norn does not adopt an opaque binary store.
- **Hybrid semantic search is the headline differentiator, pre-cleared.** The value is not "norn does embeddings too" — it is **structured filter + semantic rank in one query** (`--eq type:decision --eq workspace:norn --near "storing markdown files"`), which neither a pure semantic engine (no frontmatter awareness) nor norn-today (no rank) can do alone. Fits as a `(vector type, ANN index, similarity operator)` pillar, embeddings in the cache via a *pinned local* model (deterministic-at-query + offline; a remote embedding API breaks both), **opt-in** so core `validate`/`find` stay model-free and headless-CI-safe. This **reverses** the prior scope-ledger `⛔` "semantic search is a neighboring recall tool's job," which missed the frontmatter+semantic combination. Refines rather than crosses that boundary: the neighboring tool stays zero-config recall / candidate embedding provider; norn owns the schema-integrated hybrid query. Its first real *model dependency* — the one genuine cost. Motivation: norn as a first-class, git-backed, multimodal **agent memory system** (with `blob`). Brainstorm before build.
- **Blob is the multimodal bridge, pre-cleared.** A non-document file referenced from a Record — norn indexes relationship + lightweight metadata (size/mime/hash) only, never content, and can `fetch` the bytes. Paired with semantic search it makes agent memory multimodal, and it lets an off-filesystem agent retrieve what it cannot read directly. Content-level understanding of a blob would only ever arrive via the semantic-search pillar over a vision embedding — far future.
- **Multi-format is pre-cleared as adapters.** The Record/field model decouples from the Markdown adapter; JSON/YAML/TOML are candidate future formats (file = record), CSV/JSONL the table-shaped boundary case (file = table). Design-time obligation now: don't let rules, query grammar, or the cache assume Markdown+frontmatter is the only input. Link/heading/body features stay adapter-specific.
- **Vocabulary.** "Engine" is reclaimed as the deliberate *form* word; "Substrate" stays the *mission* word — different altitudes, do not conflate (glossary updated). "Database" stays avoided (implies opaque binary storage).
- **Not a 1.0 freeze.** A first-party *pinned* consumer hardens the confirmed contract surface (the `find`/`count` grammar, `--col`, the rules schema, the MCP tool catalog) without a general freeze. Churn stays cheap everywhere else.

## Relation to prior decisions

- Refines, does not overturn, the northstar in *the norn workspace brief*: same mission, named form.
- Consistent with [0002](./0002-link-syntax-neutrality.md) — the link graph is a first-class relational feature (joins/foreign keys), and stays syntax-neutral.
- Future pillars (semantic search, blob, multi-format adapters) are recorded here as *pre-cleared but unscheduled*; each still needs its own shaping before build, and should be run through *the scope ledger (internal design doc)*.
