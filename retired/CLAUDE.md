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

- [ ] `src/frontmatter.rs`, `src/frontmatter/` → frontmatter parse/serialize/minimal-edit (the byte-splicing invariant, ADR 0008)

### Domain model, graph, query → `norn-core`

- [ ] `src/core.rs`, `src/core/` → domain model
- [ ] `src/graph.rs`, `src/graph/` → vault graph
- [ ] `src/links.rs`, `src/links/` → link model + resolution
- [ ] `src/query.rs`, `src/filter.rs`, `src/filter_args.rs`, `src/validate_filter.rs` → query/predicate layer
- [ ] `src/grammar.rs` → canonical-form + forgiving-input grammar (ADR 0010)
- [ ] `src/standards.rs`, `src/standards/` → standards pack / rules
- [ ] `src/target.rs` → target resolution

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
- [ ] `src/set/`, `src/new/`, `src/edit/`, `src/move/`, `src/delete/`, `src/rewrite_wikilink/` → mutation verbs
- [ ] `src/env/` → VaultEnv (value-in value-out; no ambient reads in the new world)

### Config → `norn-config`

- [ ] `src/config/`, `src/config_loader.rs` → vault config (central config + registry are new-world, spec-driven)
- [ ] `src/init.rs` → vault init scaffolding

### Wire + client → `norn-wire` / `norn-client`

- [ ] `src/route_wire.rs` → Params/Report wire vocabulary → `norn-wire`
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
