# retired/ — the pre-rewrite tree. Documentation, not source.

**This directory does not build, is excluded from CI, and is never a compile
target.** It is the donor tree for the ADR 0018 greenfield rewrite
([docs/decisions/0018-greenfield-rewrite-oracle-parity.md](../docs/decisions/0018-greenfield-rewrite-oracle-parity.md)):
read it to understand how the old system behaved, port what is sound into the
new crates under `crates/`, and tick the burn-down below as modules are
subsumed. When every box is ticked, this directory is deleted (graduation).

Rules of engagement:

- **Never edit code here.** No fixes, no refactors, no formatting. A bug found
  here is either already fixed in the new code, or evidence for the divergence
  ledger — not a reason to touch the donor.
- **Never depend on it.** Nothing in `crates/` may reference `retired/` in any
  form. The wire contract with old behavior is the parity harness against the
  pinned release binary — not this source.
- **Porting means rewriting into the new boundaries**, using this tree as
  reference for semantics and edge cases. Behavior differences are allowed only
  through the divergence ledger.
- **This burn-down is the single porting tracker.** Work-tracker tasks point at
  it; they do not duplicate it. Tick items in the same PR that ports them, and
  correct a wrong destination in place — destinations below are provisional
  best guesses, not decisions.

## Burn-down

Format: `- [ ] old path → provisional destination`. Verb modules port as
`Params`/`execute`/`Report` seams (the 0016 vocabulary, carried forward).

### Text layer → `norn-frontmatter`

- [x] `src/frontmatter.rs`, `src/frontmatter/` → frontmatter parse/serialize/minimal-edit (the byte-splicing invariant, ADR 0008) → `norn-frontmatter` (NRN-339). Text layer also absorbed the syntax slices of the links/heading/section modules below — see their parentheticals.

### Domain model, graph, query → `norn-core`

- [x] `src/core.rs`, `src/core/` → domain model → `norn-core::domain` (NRN-340): `Document`, `GraphIndex`, `VaultFile`, `DocumentSummary`, the `Link` value types, and the `display` string helpers. `Heading`/`SourceSpan`/`Diagnostic` are consumed from `norn-frontmatter` (ported there in NRN-339), not redefined.
- [x] `src/graph.rs`, `src/graph/` → vault graph → `norn-core::graph` (NRN-341): `build_index_with_options` (the walk + parse + resolve pipeline), `graph_visible_markdown_under`, `is_markdown`/`is_ignored`, `concise_diagnostics`/`has_errors`, `aliases::parse_aliases`, and the ignore-glob `pattern` matcher. `IndexOptions` carries only `ignore` + `alias_field`; the donor's cache-index fields (`auto`, `resolved_index_set`, `resolved_index_set_hash`) are cache-writer-only and deferred to the cache-engine port.
- [x] `src/links.rs`, `src/links/` → link model + resolution → `norn-core::links` (NRN-341): `markdown` (Markdown `[text](url)` / `![alt](url)` extraction, the `Link` half of the donor's `commonmark`), `wikilink` (maps `norn-frontmatter` wikilink tokens into `domain::Link` records, re-basing spans to content-absolute), and all of `resolve` (path/stem/alias matching, ambiguity, anchor/block validation). The wikilink SYNTAX (`wikilink` token recognition, `anchor` split/slug/block-id, `commonmark` heading parsing) was ported to `norn-frontmatter` in NRN-339 and is consumed here; the `Link` VALUE types moved to `norn-core::domain` in NRN-340. Contract-shaped resolution quirks on the correctness slate (block-ids in code spans NRN-350; alias/ambiguity NRN-122/124) preserved as-is.
- [x] `src/query.rs`, `src/filter.rs`, `src/filter_args.rs`, `src/validate_filter.rs` → query/predicate layer (NRN-342, partial): the SQL-agnostic predicate model (`DocumentQuery`), the `FilterParams`→query parser (`build_document_query`, with the `today` clock injected value-in rather than read from `Local::now()`), the CLI filter-option adapter (`document_query_from_options`), and the validate-rule scope narrower (`rule_scope_query`) ported to `norn-core::query`. Left behind: SQL emission (`json_path_for` + `documents_matching` + string-membership SQL + the path-glob post-pass) → cache-engine port; `resolve_links_to` (needs the warm cache) → read-verb port. `validate_filter.rs` (`filter_findings`) is NOT ported — it filters `Finding`/`FindingBody`, which are phase-3 validate-engine types not yet in `norn-core`; it ports with that engine.
- [x] `src/grammar.rs` → canonical-form + forgiving-input grammar (ADR 0010) → `norn-core::grammar` (NRN-342): `split_field_value` (separator forgiveness), `normalize_argv` + the dynamic-predicate desugar / alias pack, `gate_dynamic_fields`, and `schema_field_names` (the vault-model half of the field universe). The clap-derived known-flag surface is injected as a `KnownFlags` value (norn-core never links clap); the clap-derivation itself stays in `norn-cli`. Left behind: the daemon-side dynamic-field refusal carrier (`DynamicFieldRefusal`, `gate_dynamic_refusal`, `field_universe`) — MCP `structuredContent` transport plumbing that needs the warm `Cache` + `schemars`, deferred with the cache/daemon ports.
- [ ] `src/standards.rs`, `src/standards/` → standards pack / rules (partial: the DECLARATION side ported to `norn-core::standards` in NRN-340 — `config.rs` rules model + parse/compile, `path_match.rs` (incl. `effective_match_glob`), `duration.rs`, the field-type/selector `predicates.rs` slice, and the `{{…}}` reference-scan carved from `defaults.rs` as `template_refs`; the minimal-edit splice core of `apply.rs` went to `norn-frontmatter::edit` in NRN-339. Still here for the phase-3 mutation port: the validate `engine.rs`/`checks.rs`/`findings.rs`/`index_policy.rs`, `repair/`, `substitution.rs`, `summary.rs`, the `defaults.rs` resolver (`applicable_rules`/`merge_defaults`/`resolve_to_fixpoint`), the document-matching predicates, and the verb-level apply machinery.)
- [x] `src/target.rs` → target resolution → `norn-core::target` (NRN-340): `resolve_target_path` + `backlinks`.

### Cache engine (owner-opened module in `norn-core`)

- [ ] `src/cache.rs`, `src/cache/` → schema, EAV, writer, generational contexts (ADRs 0003/0004/0013/0014)
- [ ] `src/seq_alloc.rs` → sequence allocation
- [ ] `src/init_scan.rs` → initial scan / staging

### Plan, apply, validate, repair → `norn-core`

- [ ] `src/planner/`, `src/migration_plan.rs` → plan model (typed ops)
- [ ] `src/apply/`, `src/applier.rs`, `src/apply_report.rs` → apply + report (ADR 0015 owner sets)
- [ ] `src/validate/`, `src/repair/`, `src/repair_apply.rs` → validate/repair
- [ ] `src/audit.rs` → mutation audit trail
- [ ] `src/mutation_lock/` → mutation locking

### Verb modules (Params/execute/Report seams) → `norn-core`

- [ ] `src/find/`, `src/count/`, `src/get/`, `src/describe/` → read verbs
- [ ] `src/set/`, `src/new/`, `src/edit/`, `src/move/`, `src/delete/`, `src/rewrite_wikilink/` → mutation verbs (partial: `edit/transform.rs`'s `SectionSpan` + `resolve_section` heading→span primitive already ported to `norn-frontmatter::section` in NRN-339; the edit-op grammar and the verbs themselves remain)
- [ ] `src/env/` → VaultEnv (value-in value-out; no ambient reads in the new world) (partial: the value-carrier shape from `env/mod.rs` ported to `norn-core::env` in NRN-340 — vault root + injected `VaultConfig`/`CompiledConfig`, no ambient reads. Deliberately left behind for the cache-engine / `norn-owner` ports: warm/cold `Mode`, the held-open `Cache` + generations + read pool + writer queue + per-request refresh pipeline (`env/generation.rs`, `env/refresh.rs`, `env/ensure.rs`, `env/request_scope.rs`, `env/error.rs`), and config *resolution* from disk (`config_loader`).)

### Config → `norn-config`

- [ ] `src/config/`, `src/config_loader.rs` → vault config (central config + registry are new-world, spec-driven)
- [ ] `src/init.rs` → vault init scaffolding

### Wire + client → `norn-wire` / `norn-client`

- [x] `src/route_wire.rs` → Params/Report wire vocabulary → `norn-wire`
- [ ] `src/dispatch.rs` → dispatch/summon decision → `norn-client` (the routed-or-direct question is dead; only summon-or-connect survives)

### Daemon → `norn-owner` / `norn-mcp`

- [ ] `src/serve/` → host loop, generations, writer queue, read pool → `norn-owner`
- [ ] `src/mcp/` → MCP tool surface → `norn-mcp` (handlers were already the canonical verb copies)

### CLI surface → `norn-cli`

- [ ] `src/cli.rs`, `src/help/`, `src/output/`, `src/prompt.rs` → thin adapter: parse + present only, one command-module pattern, one display-helper layer
- [ ] `src/completions/` → shell completions
- [ ] `src/self_update/` → self-update
- [ ] `src/service/` → launchd supervision verbs (owner-adjacent; destination may become `norn-owner`)
- [ ] `src/telemetry/` → telemetry (re-evaluate scope during port)

### Entry + manifests (reference only; superseded by the workspace)

- [ ] `src/main.rs`, `src/lib.rs` → composition → `norn` (bin)
- [ ] `Cargo.toml`, `Cargo.lock`, `build.rs` → dependency + build reference for the new crates
- [ ] `tests/` → semantics reference; the new world's correctness spine is the parity harness + per-crate tests, not a port of this suite
