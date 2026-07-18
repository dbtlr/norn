---
title: "Glossary"
description: "Canonical definitions of norn's load-bearing domain terms and preferred vocabulary."
---

# Glossary

Canonical definitions of norn's load-bearing domain terms — the vocabulary *the norn workspace brief* and *the scope ledger (internal design doc)* lean on. Opinionated by design: when several words mean the same thing, the preferred term is defined and the rest listed under _Avoid_.

## Language

**Vault**
A directory tree of Markdown files carrying frontmatter and links — norn's unit of operation. Generic over Obsidian and other tools; norn never requires Obsidian to be present.
_Avoid_: notebook, knowledge base, repo, library.

**Link**
A reference from one document to another. norn recognizes two syntaxes: relative Markdown links (`[label](../notes/foo.md)`) and Obsidian wikilinks (`[[target]]`). Relative Markdown links are the default idiom in norn's docs and examples; wikilinks are fully supported but opt-in — see [0002](./decisions/0002-link-syntax-neutrality.md). Both participate equally in resolution, queries, validation, and move/delete cascades.
_Avoid_: reference, pointer; "backlink" only for an incoming link specifically.

**Drift**
A vault's divergence from its declared standards: missing or invalid frontmatter, broken or unresolved links, files in the wrong place. The thing norn exists to detect (`validate`) and heal (`repair`).
_Avoid_: rot, mess, inconsistency (too vague to act on).

**Turn**
One agent tool-call round-trip. norn measures part of its value in turns saved — collapsing a multi-call grep/jq/sed dance into a single deterministic command.
_Avoid_: step, call, hop.

**Pipeline stage**
What norn refuses to be for *daily* operations: a fragment useful only when composed with jq/grep/sed. norn does filter / sort / limit / page / column-selection / grouping natively. One-off auditing may still pipe — see the northstar.
_Avoid_: filter, transform (when naming the anti-pattern).

**Substrate**
norn itself — the deterministic graph of documents, links, headings, and frontmatter that both humans and agents operate on. The **mission** word (the altitude of _what norn is for_). Distinct from **Engine**, the **form** word (_what shape norn takes_) — see [0003](./decisions/0003-relational-engine-over-flat-files.md). Do not conflate the two altitudes.
_Avoid_: backend, store.

**Engine**
norn as a relational query/mutation engine whose tables are flat, human-readable text files — SQLite's query/index/transaction semantics, git's durability, Obsidian's inspectability. More precisely a **document store + link graph + relational constraints, over flat files**: the flat-file, git-versioned, inspectable member of the SQLite/Postgres/Mongo family, which supplies primitives to _reason with_, not clone targets. The **form** word; grows by absorbing pillars (a `(type, index, operator)` triple, or an adapter), never by forking paradigms. See [0003](./decisions/0003-relational-engine-over-flat-files.md).
_Avoid_: database (implies opaque binary storage — norn's tables stay readable files).

**Record** _(a.k.a. Document)_
One flat file treated as a row: structured fields plus an optional free-text body. The engine's unit of storage. Markdown+frontmatter is the first record format; JSON/YAML/TOML (file = record) and CSV/JSONL (file = table) are pre-cleared future formats, not current scope.
_Avoid_: markdown file (too format-specific for the core model), row (only inside the DB analogy).

**The three layers**
How norn is a database made of flat files: **source files are the tables** (human-readable, diffable, the source of truth) · **the SQLite cache is the query engine and index** (derived, disposable, rebuildable — the _only_ place normalization, expression indexes, and embeddings are allowed to live) · **git is the write-ahead log** (transaction history and the undo button). "Delete the directory, let norn validate/repair, revert if wrong" is this model in one sentence.

**Adapter**
The parser that maps a text format into the Record/field model. Markdown+frontmatter is the first and richest adapter; its body / link / heading features are adapter-specific, not core. JSON/YAML/TOML, CSV, and JSONL are candidate future adapters — none current scope.
_Avoid_: parser, loader (when naming the format-to-record boundary specifically).

**Blob**
A non-document file referenced from a Record (e.g. an image embedded in Markdown). norn indexes only the **relationship** (via the link graph) plus lightweight metadata (size, mime, hash) — never the blob's content — and can **fetch** the bytes on demand. The external/large-object pillar; the piece that lets an off-filesystem agent retrieve what it cannot read directly. Pre-cleared future, not current scope.
_Avoid_: attachment, asset (fine in prose; Blob is the model term).

**Standards pack**
A user-defined set of rules in `.norn/config.yaml` (required fields, allowed values, link and location constraints) that makes a vault's local doctrine mechanically enforceable. Each real-world vault defines its own pack; none is baked into the core.
_Avoid_: schema (overloaded), config, ruleset.
^standards-pack

**norn-service (the warm daemon)**
A persistent, single-owner process that opens the vault's SQLite cache once, integrity-verifies it, and holds it *warm* for its lifetime — so verification is paid once, not per invocation. It is the existing `norn mcp` server upgraded from a stdio one-shot into a listener. See [0005](./decisions/0005-trusted-cache-via-warm-service.md), *the norn-service architecture design doc (internal design doc)*.
_Avoid_: server (ambiguous with the MCP tool surface), watcher (a later, separable component of the service).

**Routing seam**
The decision point in `norn-cli` that, per invocation, probes for a live `norn-service` and either delegates the whole operation to it (translating args to the MCP contract) or falls back to running directly. The seam is what lets trust be *inherited or re-established*, never skipped.
_Avoid_: proxy, dispatch (too generic).

**Inherited trust**
The property that a routed request need not re-verify the cache because it is served by a process that already holds a live, verified cache — as opposed to *re-established* trust, where a direct CLI invocation pays its own integrity check. Liveness is proven per-request (a prompt handshake), not stamped as a lease, so there is no stale-trust window.
_Avoid_: cached trust, trusted mode.

**Ignore** _(`files.ignore`)_
A path excluded from norn entirely: the parser never reads it, so it is not indexed, not a graph member, not a resolvable link target, and not validated. Links *into* an ignored path resolve to nothing (`link-target-missing`) — the target is outside norn by definition. The tier for content norn should not know about (e.g. `Archive/**`, so archived stems stop shadowing live hubs). See [0007](./decisions/0007-two-tier-ignore-model.md).
_Avoid_: exclude, skip (when naming the config tier); never conflate with **Validation-exempt**.

**Validation-exempt** _(`validate.ignore`)_
A path that is fully **indexed** and in the graph — frontmatter queryable via `find`/`get`, resolvable as a link target, valid outbound links are edges — but excluded from **validation**: no frontmatter-schema findings, no broken-link findings. Broken links inside it are stored but never reported; its frontmatter need not satisfy the standards pack. Full-file exemption only — norn has no per-finding-code path suppression, deliberately. The tier for *frozen records* norn should track but not police (`artifacts/session-logs/**`, `artifacts/scratch/**`). See [0007](./decisions/0007-two-tier-ignore-model.md).
_Avoid_: frozen (informal, and misleads toward immutable — exempt records stay mutable), ignore (that is the other tier).

**Indexing vs Validation**
Two independent operations norn performs on a record. **Indexing** parses a record's frontmatter and links into the cache, making its fields queryable and its links part of the graph. **Validation** checks an already-indexed record against the standards pack. `validate.ignore` turns validation *off* while leaving indexing *on* — which is why a validation-exempt session-log stays queryable (`find type=session-log`) even though it is never policed. Conflating the two is the error that makes "unvalidated" seem to imply "invisible."
_Avoid_: (a distinction, not a synonym cluster — keep both words.)
**Canonical form**
The single authoritative spelling of a CLI grammar — the one docs teach, `--help` shows, errors echo, and outputs render. Every grammar has exactly one; forgiven variants normalize to it and never appear in documentation. See [0010](./decisions/0010-cli-grammar-forgiving-inputs.md).
_Avoid_: default form, preferred syntax.

**Forgiving input**
The input-side complement to Canonical form: the parser accepts predictable variants — separator variance (`:` vs `=`), common aliases (`--group-by` for `--by`), dynamic field predicates — and silently normalizes them to canonical. A variant is admitted only on mined evidence of real agent guesses: accepted, never taught. See [0010](./decisions/0010-cli-grammar-forgiving-inputs.md).
_Avoid_: lenient parsing, fuzzy matching (forgiveness is deterministic).

**Field-universe gate**
The guardrail that keeps dynamic field predicates trustworthy: an unknown `--key value` is interpreted as a predicate only when the key resolves against *this vault's* known fields (schema-declared ∪ observed frontmatter keys); otherwise it hard-errors with did-you-mean across both flags and fields. Exists to prevent typo-swallowing — a mistyped real flag must never masquerade as a valid empty query. See [0010](./decisions/0010-cli-grammar-forgiving-inputs.md).
**Owner set**
The exact, sorted vault-relative paths of every Record matching one logical selector at a point in time. A plan owner-set precondition compares the planning snapshot with the under-lock apply snapshot; a mismatch refuses before writes.
_Avoid_: reservation, uniqueness lock, target count.
**Published cache snapshot**
One relational cache revision visible to a structured tool call: document rows, fields, headings, blocks, diagnostics, and globally resolved links publish together, and the request stays pinned to that revision.
_Avoid_: intermediate state, partial snapshot, current generation (a generation can contain multiple published revisions).

**Document Source**
The exact UTF-8 text of one Record in the source layer. `get --format markdown` is its retrieval surface; it is not a structural facet or part of the Published cache snapshot.
_Avoid_: raw, raw bytes, source facet.
**Adapter**
A surface that parses external input into the canonical Request vocabulary and presents the typed Report back out — clap (CLI), rmcp framing (MCP), future HTTP or napi. Adapters own parsing and presentation ONLY; orchestration is core's. See [0016](./decisions/0016-surface-neutral-command-core.md).
_Avoid_: frontend, surface layer (too vague), "the MCP layer" when meaning the handlers (those are core).

**Dispatch**
The single per-request routing decision: run `execute` against a cold in-process env, or ship the Request over the socket to the warm daemon. Happens exactly once, in the process that received the user's intent; the daemon end never re-dispatches. Lives in norn-core so embedders inherit it.
_Avoid_: router/routing layer (implies a standing component; it's one decision), proxy.

**Engine layer / Command layer**
The two strata inside norn-core. The **engine** is the typed stratum — VaultEnv, graph/cache, schema validation on typed values, preflight → plan → apply, typed Reports; the future embeddable SDK surface. The **command layer** is the Request/Report vocabulary + per-verb `execute()` that desugars the token grammar (`KEY=VALUE` / `KEY=JSON`) into typed engine ops. Rule: typed-primary — tokens desugar; the token grammar is never the only way in.
_Avoid_: core vs shell (ambiguous with the crate names), business logic.
**Summoned owner**
The daemon process that exclusively owns a vault's cache while alive. Summoned on demand by any invocation (never administered), it holds the entry's lifetime flock, carries the burden of proof between requests, and reaps itself after an idle TTL. See [0017](./decisions/0017-registered-vaults-summoned-owners.md).
_Avoid_: server, service (reserve for the managed tier's supervised install), background process.

**Lifecycle ladder**
The three daemon tiers — ephemeral (unregistered vaults, dev builds, tests; tmp cache), resident (registered vaults, summoned; persistent entry), managed (supervised, no TTL; HTTP MCP) — differing only in cache home, keep-alive, and GC aggressiveness. One codebase, one execution path.
_Avoid_: modes, deployment options.

**Drift auditor**
The owner's truth loop: periodically reconciles the warm cache against filesystem reality and measures watcher fidelity as a health metric. Drift ≈ 0 licenses "a warm vault is 100% trusted"; drift > 0 is a watcher bug surfacing as a metric instead of as wrong query results. The "don't skip" half of relocating verification off the request path.
_Avoid_: validator (reserved for norn validate, which checks vault standards, not cache trust).

**Registration-gated durability**
The principle that all durable vault artifacts — persistent cache entry, event stream, logs, health history — are benefits of explicit registration. Unregistered work is tmp-homed and disposable by definition. Answers every future "where does artifact X live" question.
_Avoid_: opt-in persistence (undersells that the gate covers all artifacts, not just cache).
**Parity oracle**
The pinned released binary used as the executable specification during the registered-vault rewrite: same argv, same fixture vault — its outputs define "1:1 interfaces" for every ported surface. Post-switchover the parity suite becomes the standing release-to-release regression harness.

**Divergence ledger**
The in-repo record of intended old→new behavior differences during the rewrite. Each entry names the surface, both behaviors, and the reason — decided-better, or discovered-inconsistency between old commands. The parity runner has exactly three verdicts: match, diverged-with-ledger-entry, drift (fails).

**Retired code**
The pre-rewrite tree moved to `retired/`: CI-excluded, non-building, documentation-not-source, with the porting burn-down list inside as the single tracker of what remains to move. Deleted at graduation.

**Graduation gate**
The stability bar for the rewrite to replace the stable line: full parity suite green, divergence ledger fully adjudicated, beta prereleases cut from the rewrite branch, daily real-vault use on the beta, then the stable switchover.

**Fixture-vault generator**
Deterministic, seed-driven generation of test vaults (schema variety, link topologies, frontmatter edge cases) — the reproducible substrate for the parity harness and contributor CI, replacing reliance on any single private real-world vault.
