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
- [x] `src/query.rs`, `src/filter.rs`, `src/filter_args.rs`, `src/validate_filter.rs` → query/predicate layer (NRN-342, partial): the SQL-agnostic predicate model (`DocumentQuery`), the `FilterParams`→query parser (`build_document_query`, with the `today` clock injected value-in rather than read from `Local::now()`), the CLI filter-option adapter (`document_query_from_options`), and the validate-rule scope narrower (`rule_scope_query`) ported to `norn-core::query`. Left behind: SQL emission (`json_path_for` + `documents_matching` + string-membership SQL + the path-glob post-pass) → cache-engine port; `resolve_links_to` (needs the warm cache) → read-verb port. `validate_filter.rs` (`filter_findings` + `ValidateFilterOptions`) ported with the validate engine in NRN-381 → `norn-core::standards::validate_filter` (the donor's clap `From<&ValidateArgs>`/`From<&RepairArgs>` adapters dropped — the CLI/owner builds the options from the wire params).
- [x] `src/grammar.rs` → canonical-form + forgiving-input grammar (ADR 0010) → `norn-core::grammar` (NRN-342): `split_field_value` (separator forgiveness), `normalize_argv` + the dynamic-predicate desugar / alias pack, `gate_dynamic_fields`, and `schema_field_names` (the vault-model half of the field universe). The clap-derived known-flag surface is injected as a `KnownFlags` value (norn-core never links clap); the clap-derivation itself stays in `norn-cli`. Left behind: the daemon-side dynamic-field refusal carrier (`DynamicFieldRefusal`, `gate_dynamic_refusal`, `field_universe`) — MCP `structuredContent` transport plumbing that needs the warm `Cache` + `schemars`, deferred with the cache/daemon ports.
- [ ] `src/standards.rs`, `src/standards/` → standards pack / rules (partial: the DECLARATION side ported to `norn-core::standards` in NRN-340 — `config.rs` rules model + parse/compile, `path_match.rs` (incl. `effective_match_glob`), `duration.rs`, the field-type/selector `predicates.rs` slice, and the `{{…}}` reference-scan carved from `defaults.rs` as `template_refs`; the minimal-edit splice core of `apply.rs` went to `norn-frontmatter::edit` in NRN-339. The mutation+validate SHARED remnant ported in NRN-376: `substitution.rs` (the `{{…}}` renderer, transform table now the single source of truth pinned equal to `template_refs::KNOWN_TRANSFORMS`), the `defaults.rs` resolver (`applicable_rules`/`merge_defaults`/`resolve_to_fixpoint`, clock injected value-in — no `Local::now()`), and the document-matching predicates (`document_frontmatter_field`/`document_has_frontmatter_field`/`frontmatter_predicates_match`) folded into `predicates.rs`. The READ-side validate engine ported in NRN-381: `findings.rs` (the `Finding`/`FindingBody`/`Severity` model), `checks.rs`, `engine.rs` (`validate_with_compiled` — the single blessed entry; the donor's uncompiled `validate`/`validate_rule*` wrappers are retained only as `#[cfg(test)]` helpers), `index_policy.rs` (already landed as `resolved_index_set`), and `summary.rs` (`summarize`) → `norn-core::standards`; plus `compile_config` carved from `parse_config_compiled` so validate pre-compiles once. The MUTATION engine ported in NRN-386: `repair.rs` (the `RepairPlan`/`PlannedChange` model + `plan_repairs` generator + the `repair/` leaf helpers `closest_match`/`link_risk`/`warnings`/`destination`) → `norn-core::standards::repair`, and `apply.rs`'s verb-level file-mutation primitives (the rich `ApplyError`/`ContainmentError`, `ensure_within_vault`, `apply_file_changes` minimal-edit, `apply_move`/`apply_delete`/`apply_link_rewrites`/`apply_replace_body`/`apply_edit_ops`/`apply_rewrite_link`, `RepairApplyReport`/`CascadeRecord`) → `norn-core::standards::apply`. Still here: only the CLI render `repair/` and the mutation-verb surfaces themselves.)
- [x] `src/target.rs` → target resolution → `norn-core::target` (NRN-340): `resolve_target_path` + `backlinks`.

### Cache engine (owner-opened module in `norn-core`)

- [x] `src/cache.rs`, `src/cache/` → schema, EAV, writer, generational contexts (ADRs 0003/0004/0013/0014) → `norn-core::cache` (NRN-344): schema DDL + EAV `document_fields` writer, canonical, change-detection + `FreshnessProbe` trust seam, `full_build`/incremental refresh + dormant chunked increment pipeline, the read surface (`load_graph_index`, `documents_matching`/`find_documents` EAV-scan router with EXPLAIN guards, `document_with_connections`, status), the two-class `WriterQueue`, generational `ReadPool`, and the owner-facing `VaultCacheSlot` (`create`/`ensure_current`/`serve_read`/coalesced refresh/`commit_apply_increments`). Deleted per ADR 0017: identity path matrix, channels, self-heal ladder, reshred-on-open, GC/prune, `(dev,ino)` re-verification + sentinel + invalidation floor, and `PublicationAuthority`'s filesystem double-re-proof. `src/seq_alloc.rs` (phase 3) and `src/init_scan.rs` (unrelated `norn init` tally) remain.
- [x] `src/seq_alloc.rs` → sequence allocation → `norn-core::seq_alloc` (NRN-377): filesystem max+1 `{{seq}}` id allocation (NRN-101), `SEQ_TOKEN` inlined (the `new::generate` const is not ported). The load-bearing single-writer coupling is carried in the module docs, rebased from the pre-owner cross-process `flock` onto the owner's in-process writer queue (ADR 0013/0017); the donor's `resolve_seq`/`seq_misplaced` tests port as-is, plus a `resolve_seq_create` fold-in-prior-allocations test.
- [ ] `src/init_scan.rs` → initial scan / staging

### Plan, apply, validate, repair → `norn-core`

- [ ] `src/planner/`, `src/migration_plan.rs` → plan model (typed ops) (partial: `migration_plan.rs` ported whole to `norn-core::plan` in NRN-377 — `MigrationPlan` v2, the `MigrationOp`/`PlanPrecondition`/`OwnerSelector` model, `SkippedFinding`, and the content-addressed `canonical_hash`; round-trip + owner-selector-grammar tests port as-is. `src/planner/` ported in NRN-386 → `norn-core::planner`: `intent/` (the `move_folder`/`rewrite_wikilink` expanders + `expand`/`HIGH_LEVEL_KINDS`, now live via the executor) and `findings/` (`plan_from_findings`, the validate-`Finding`→`MigrationPlan` adapter, live once the `repair` verb lands).)
- [ ] `src/apply/`, `src/applier.rs`, `src/apply_report.rs` → apply + report (ADR 0015 owner sets) (partial: `apply_report.rs` report TYPES ported to `norn-core::apply::report` in NRN-377 — `ApplyReport` v3, ops/preconditions/cascade/`LinkImpact`, the `ApplyOutcome`→exit mapping, the coded error/warning envelopes, and `refused()`; the CLI/MCP error-DOWNCAST glue (`from_rich`/`from_anyhow`) and wire reconstruction (`reconstruct_wire_report`/`emit_refusal`) stay with the surface crates that own those error types and do the rendering. The ADR 0015 owner-set EVALUATION engine carved from `applier.rs` → `norn-core::apply::preconditions` (`evaluate_owner_preconditions` + the single canonical `build_owner_precondition_refusal_report`). The `applier.rs` pass-based EXECUTOR body ported in NRN-386 → `norn-core::apply::executor` (`apply_migration_plan` + `ApplyContext`): typed-op expansion via `planner::intent`, delegation to `apply::repair_apply`, the `standards::apply` primitives, `seq_alloc` create-path barrier wiring via `resolve_create_paths`, telemetry emission, and the cache-write-through `touched_paths` carrier. The duplicated owner-precondition engine + local owner-refusal builder were dropped for the single landed `apply::preconditions` copy, and the donor's ~four hand-synced `ApplyReport` builder sites collapsed to one canonical `assemble_report` constructor (dry-run/apply count parity by construction). The from_rich/from_anyhow envelope glue landed engine-local as `apply::envelope` (engine-owned error types only; surface-verb codes stay with the verbs). Still here: only `src/apply/` (the `norn apply` CLI verb — prompt/routing/stdin).)
- [ ] `src/validate/`, `src/repair/`, `src/repair_apply.rs` → validate/repair (partial: the READ-side `validate` verb ported in NRN-381 — `src/validate/` render logic became the `norn-cli` display layer's `render_validate` + the severity-tally/`tally_group`/`status_headline` primitives + `fix_hints`, the engine went to `norn-core::standards` (see the standards line above), and the verb is a `norn-core::read::validate` execute seam wired through the routed owner like the other read verbs. `repair_apply.rs` ported in NRN-386 → `norn-core::apply::repair_apply` (`apply_repair_plan_with_context` + `CreateApplyContext`) — the pass-based apply ORCHESTRATOR the executor delegates to; its one move-verb-driven test was rewritten to build the plan directly via the ported `classify_link_risk`. Still here: only the CLI render `src/repair/` (`render`/`skip_reasons`), which ports with the `repair` verb surface.)
- [ ] `src/audit.rs` → mutation audit trail (deferred with telemetry: `audit.rs` is the `norn audit` READ verb over the telemetry event stream (`crate::cli::AuditArgs` + `telemetry::read`), not engine — it ports with the telemetry + CLI surface, not the mutation-engine substrate.)
- [ ] `src/mutation_lock/` → mutation locking (deferred/superseded: the cross-process advisory `flock` (`MutationLock`) is subsumed by the owner's in-process single-writer queue (ADR 0013/0017) — `fs2` now lives in `norn-owner`, not `norn-core` — and the `pending` content-addressed stash is `norn apply` verb surface. The seq_alloc↔single-writer coupling that motivated it is carried as a documented invariant in `norn-core::seq_alloc`. Any residual flock lands with the executor/verb caller that would acquire it, in the owner layer.)

### Verb modules (Params/execute/Report seams) → `norn-core`

- [ ] `src/find/`, `src/count/`, `src/get/`, `src/describe/` → read verbs
- [ ] `src/set/`, `src/new/`, `src/edit/`, `src/move/`, `src/delete/`, `src/rewrite_wikilink/` → mutation verbs (partial: `edit/transform.rs`'s `SectionSpan` + `resolve_section` heading→span primitive already ported to `norn-frontmatter::section` in NRN-339; the section-edit TRANSFORM ENGINE (`edit/ops.rs` `EditOp` + `edit/transform.rs` `apply_edits`) ported in NRN-386 → `norn-core::edit` (the executor's compose path needs it); the edit-op grammar's VERB surface (`edit/route`/`report`/`sugar`/`synth`) and the verbs themselves remain)
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
- [ ] `src/telemetry/` → telemetry (partial: the in-memory mutation event stream the executor emits through and folds into an `ApplyReport` ported in NRN-386 → `norn-core::telemetry` — `event` (Event/Severity + constants), `ids` (IdGen on BLAKE3 / Clock), and the `EventSink` with a value-in durable-writer seam. Still here: the durable daily-file JSONL `store` and the `norn audit` READ verb (`read.rs`), which port with the audit + CLI surface.)

### Entry + manifests (reference only; superseded by the workspace)

- [ ] `src/main.rs`, `src/lib.rs` → composition → `norn` (bin)
- [ ] `Cargo.toml`, `Cargo.lock`, `build.rs` → dependency + build reference for the new crates
- [ ] `tests/` → semantics reference; the new world's correctness spine is the parity harness + per-crate tests, not a port of this suite
