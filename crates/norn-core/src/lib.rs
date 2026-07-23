#![forbid(unsafe_code)]
//! Domain model, verb seam (Params/execute/Report), plan/apply, validation, and the cache engine — value in, value out.
//!
//! May never: Touch sockets, clap, rmcp, the central config, process spawning, or ambient env/XDG/CWD resolution — all roots and paths arrive as values.
//!
//! # Surface (ported from the pre-rewrite tree, ADR 0018)
//!
//! - [`domain`] — the serializable graph vocabulary: [`domain::Document`],
//!   [`domain::GraphIndex`], the [`domain::Link`] model, and the diagnostic /
//!   heading / span types re-exported from `norn-frontmatter`. Pure data, no I/O.
//! - [`standards`] — the standards pack / rules model AND both engines it feeds:
//!   the declarative [`standards::VaultConfig`] surface, YAML parse + compile
//!   ([`standards::parse_config`], [`standards::parse_config_compiled`]), path
//!   pattern matching, and the field-type / predicate declaration semantics; the
//!   validate engine ([`standards::validate_with_compiled`], the [`standards::Finding`]
//!   model, [`standards::filter_findings`]); the repair planner
//!   ([`standards::plan_repairs`]); the interior typed applier op model
//!   ([`standards::ApplyOp`] / [`standards::ApplyBatch`], the successors to the
//!   deleted `PlannedChange`/`RepairPlan`); and the low-level file-mutation
//!   primitives in [`standards::apply`].
//! - [`target`] — target resolution and backlink lookup over a built graph.
//! - [`env`] — the [`env::VaultEnv`] value-carrier: vault root plus injected
//!   config, value-in / value-out with no ambient reads.
//! - [`graph`] — the vault walk + parse pipeline
//!   ([`graph::build_index_with_options`]) producing a resolved
//!   [`domain::GraphIndex`], plus the ignore-glob and alias-field machinery.
//! - [`links`] — the link model and resolution: Markdown-link and wikilink
//!   extraction into [`domain::Link`] records and matching a link to a document
//!   ([`links::resolve_links`]).
//! - [`query`] — the SQL-agnostic predicate model ([`query::DocumentQuery`]) and
//!   its input parsing ([`query::filter_args::build_document_query`],
//!   [`query::rule_scope_query`]). Shapes queries; the cache engine runs them.
//! - [`grammar`] — the ADR 0010 canonical-form + forgiving-input grammar:
//!   separator forgiveness, the query-family dynamic-predicate desugar
//!   ([`grammar::normalize_argv`]), and the field-universe gate. clap-free; the
//!   CLI injects its known-flag surface as a value.
//! - The typed-op [`norn_wire::MigrationPlan`] model — operations, owner-set
//!   preconditions (ADR 0015), and the content-addressed canonical hash — lives
//!   in norn-wire (the end-user plan contract). A surface-neutral, serializable
//!   artifact that crosses the wire as plan bytes.
//! - [`apply`] — the one apply engine (ADR 0024): the plan-load + schema-gate +
//!   expansion + report-assembly orchestrator ([`apply::apply_migration_plan`] in
//!   `apply::executor`) driving the ordered named passes (`apply::passes`) over the
//!   narrow filesystem write primitives (`apply::fsops`) and the per-file
//!   fingerprint→shadow→verify→swap unit (`apply::transaction`). Emits the
//!   [`norn_wire::ApplyReport`] output vocabulary with per-op status and enforces
//!   the ADR 0015 owner-set precondition barrier
//!   ([`apply::evaluate_owner_preconditions`]). Partial apply is the semantics: an
//!   independent op still runs when a sibling fails; only a plan-level barrier
//!   refuses the whole plan pre-write.
//! - [`planner`] — the shared planner that turns intent into a MigrationPlan: the
//!   per-kind high-level op expanders (`planner::intent::expand`, consumed by the
//!   applier) and the validate-`Finding`→plan adapter (`planner::findings`,
//!   consumed by the `repair` verb).
//! - [`read`] — the read verbs' execute seams (`find` / `count` / `get` /
//!   `describe` / `validate` / `repair`), each a pure function of a warm
//!   [`cache::Cache`] plus a wire `Params`, producing a wire `Report`.
//! - [`mutate`] — the mutation verbs' execute seams (`set` / `new` / `edit` /
//!   `move` / `delete` / `rewrite_wikilink`): each builds — and, when confirmed,
//!   applies — a MigrationPlan against the warm cache.
//! - [`edit`] — the section-edit ENGINE (the op vocabulary + the pure body
//!   transform) the applier's compose path runs for section/body edit ops.
//! - [`cache`] — the cache engine: an owner-opened SQLite projection of the vault
//!   graph with predicate SQL emission over [`query::DocumentQuery`], paged find,
//!   deep projection, and the freshness/refresh trust seam.
//! - [`telemetry`] — the in-memory mutation event stream the applier emits
//!   through and folds into an `ApplyReport` (the durable JSONL store + the
//!   `norn audit` read verb are not here yet — see below).
//! - [`seq_alloc`] — apply-time `{{seq}}` id allocation (filesystem max+1),
//!   coupled to the writer boundary the owner holds.
//!
//! Deliberately NOT here yet (later port phases): the
//! durable daily-file JSONL telemetry store and the `norn audit` read verb over
//! it (only the in-memory event stream is ported); the `init` / `init_scan`
//! vault-scaffolding and staging surface; on-disk config *resolution* (the
//! central config home is `norn-config`'s job, injected as a value here); and the
//! owner's warm-cache serve loop — held-open cache, generations, read pool,
//! writer queue, per-request refresh — which lives in `norn-owner`, not here.

pub mod apply;
pub mod cache;
pub mod domain;
pub mod edit;
pub mod env;
pub mod grammar;
pub mod graph;
pub mod links;
pub mod mutate;
pub mod planner;
pub mod query;
pub mod read;
pub mod seq_alloc;
pub mod standards;
pub mod target;
pub mod telemetry;

/// One-line boundary contract, referenced by every dependent so each
/// declared edge in the crate map is compiler-load-bearing.
pub const CONTRACT: &str = "norn-core: domain model, verb seam, plan/apply, validation, cache engine — value in, value out";

/// Direct-dependency contracts — the code reference that makes this
/// crate's declared edges load-bearing rather than manifest-only.
pub const DEP_CONTRACTS: &[&str] = &[norn_wire::CONTRACT, norn_frontmatter::CONTRACT];
