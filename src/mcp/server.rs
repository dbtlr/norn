//! The MCP server handler.
//!
//! Task 1 is the scaffold: an empty tool router with zero `#[tool]` methods, so
//! `tools/list` answers with an empty array. Later tasks add `#[tool]` methods.
//!
//! Task 13 splits the tools into two `#[tool_router]` blocks — `read_router`
//! (the 7 read tools) and `mutate_router` (the 7 mutation tools) — merged
//! together by `McpServer::new` into one served surface (see `routers`).
//!
//! Task 2 wires in a warm [`VaultContext`] so tool implementations can call
//! `self.ctx.query_cache()` to open a fresh cache handle on each invocation —
//! getting the CLI's per-invocation freshness check without a filesystem watcher.
//!
//! We use the explicit `#[tool_handler(router = self.tool_router)]` form (rather
//! than `#[tool_router(server_handler)]`) so the generated `ServerHandler`
//! dispatches through the *instance* `tool_router` field. The `server_handler`
//! convenience variant instead routes through a fresh `Self::tool_router()` each
//! call, which would leave the field unread and trip `-D dead_code`.

use std::sync::Arc;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};

use super::mutation_result::MutationResult;
use super::notes::Noted;
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};

use super::context::{RequestScope, VaultContext};
use super::to_mcp_error;
use crate::describe::DescribeOutput;
use crate::mcp::tools::apply::ApplyOutput;
use crate::mcp::tools::audit::AuditOutput;
use crate::mcp::tools::count::CountEnvelope;
use crate::mcp::tools::delete::DeleteOutput;
use crate::mcp::tools::edit::EditOutput;
use crate::mcp::tools::find::FindOutput;
use crate::mcp::tools::get::GetOutput;
use crate::mcp::tools::move_doc::MoveOutput;
use crate::mcp::tools::new::NewOutput;
use crate::mcp::tools::repair::RepairOutput;
use crate::mcp::tools::rewrite_wikilink::RewriteWikilinkOutput;
use crate::mcp::tools::set::SetOutput;
use crate::mcp::tools::validate::ValidateOutput;

/// Tool-name constants for the per-call served markers.
///
/// The rmcp `#[tool]` macro requires a string LITERAL for `name`, so the
/// attribute cannot share these consts directly. Instead, every `run_tool` /
/// `run_wrapped` call site passes a const from this table, and
/// the `served_marker_names_match_the_advertised_catalog` test asserts [`ALL`]
/// (this table) set-equals the advertised `tools/list` catalog — so a marker
/// name that drifts from its `#[tool(name = ...)]` attribute, or a new tool
/// whose marker was forgotten here, fails deterministically.
pub(crate) mod tool_names {
    pub(crate) const GET: &str = "vault.get";
    pub(crate) const AUDIT: &str = "vault.audit";
    pub(crate) const COUNT: &str = "vault.count";
    pub(crate) const FIND: &str = "vault.find";
    pub(crate) const VALIDATE: &str = "vault.validate";
    pub(crate) const REPAIR: &str = "vault.repair";
    pub(crate) const DESCRIBE: &str = "vault.describe";
    pub(crate) const NEW: &str = "vault.new";
    pub(crate) const SET: &str = "vault.set";
    pub(crate) const EDIT: &str = "vault.edit";
    pub(crate) const MOVE: &str = "vault.move";
    pub(crate) const DELETE: &str = "vault.delete";
    pub(crate) const REWRITE_WIKILINK: &str = "vault.rewrite_wikilink";
    pub(crate) const APPLY: &str = "vault.apply";

    /// Every tool name above, for the catalog drift guard. Test-only by
    /// design: the drift-guard test is its single consumer (the runtime marker
    /// path reads the individual consts), so the cfg gate is the honest shape
    /// rather than an `allow(dead_code)`.
    #[cfg(test)]
    pub(crate) const ALL: &[&str] = &[
        GET,
        AUDIT,
        COUNT,
        FIND,
        VALIDATE,
        REPAIR,
        DESCRIBE,
        NEW,
        SET,
        EDIT,
        MOVE,
        DELETE,
        REWRITE_WIKILINK,
        APPLY,
    ];
}

#[derive(Clone)]
pub struct McpServer {
    /// Warm vault context: config held for the server lifetime; cache opened
    /// fresh per tool call via `self.ctx.query_cache()`.
    pub(crate) ctx: Arc<VaultContext>,
    /// In-process serialization lock for tool bodies.
    ///
    /// The MCP server is one long-lived process serving many `tools/call`
    /// requests on a multi-thread tokio runtime; the tool handlers run blocking
    /// SQLite work on `spawn_blocking` threads. This async mutex serializes every
    /// tool body so vault work runs one-operation-at-a-time within the process.
    ///
    /// Its remaining job is exactly that **tool-body serialization**, pending the
    /// per-generation read pool (NRN-253) that will let independent reads run
    /// concurrently. It is NO LONGER the cold-open-race guard it was introduced as
    /// (NRN-55): under warm mode every cache open — cold start, ground-shift,
    /// corruption, index-relevant config change — now flows through the
    /// generational single-flight path (`VaultContext::ensure_current`, ADR 0013,
    /// NRN-251), which coalesces concurrent opens by construction, so open safety
    /// no longer depends on this lock. (Cold stdio `norn mcp` still opens a fresh
    /// cache per call, and this lock still serializes those cold-start DDL windows
    /// for that transport — see `concurrent_cold_start_calls_all_succeed`.)
    call_lock: Arc<tokio::sync::Mutex<()>>,
    /// When true, every served tool call emits a per-call
    /// `norn serve: served <tool>` marker on stderr (NRN-94 review F6 — the
    /// routing proofs count these). Set ONLY by the warm host daemon
    /// ([`new_daemon`](Self::new_daemon)); a stdio `norn mcp` process must
    /// never write markers (they'd be mislabeled and pollute a client's stderr
    /// channel). Living in the shared `run_wrapped` funnel, the gate covers
    /// every current and future tool — a handler cannot reintroduce the leak.
    emit_serve_markers: bool,
    tool_router: ToolRouter<Self>,
}

impl McpServer {
    /// Build the server: the `#[tool]` methods are split into two routers —
    /// `read_router()` (7 read tools) and `mutate_router()` (7 mutation tools) —
    /// which this merges into one served surface (see `routers`).
    pub fn new(ctx: Arc<VaultContext>) -> Self {
        let mut routers = Self::routers().into_iter();
        // `routers` always yields at least the read router; merge the rest in.
        let mut router = routers
            .next()
            .expect("routers() always yields the read router");
        for extra in routers {
            router.merge(extra);
        }
        Self {
            ctx,
            call_lock: Arc::new(tokio::sync::Mutex::new(())),
            // Off for the stdio `norn mcp` transport: served markers are a
            // daemon-only observability channel (see `emit_serve_markers`).
            emit_serve_markers: false,
            tool_router: router,
        }
    }

    /// Build the server for the warm host daemon (`norn serve`): identical
    /// surface to [`new`](Self::new), plus the per-call served markers on
    /// stderr that the routing proofs count (see `emit_serve_markers`).
    pub fn new_daemon(ctx: Arc<VaultContext>) -> Self {
        Self {
            emit_serve_markers: true,
            ..Self::new(ctx)
        }
    }

    /// The tool routers that compose the served surface, in merge order.
    ///
    /// Single source of truth for *which* routers exist. Both [`new`](Self::new)
    /// (which merges them into the stored router) and the CLI↔MCP parity gate
    /// (`super::parity_gate`, which enumerates the full surface via this same
    /// function) consume this seam, so adding a third `#[tool_router]` block
    /// here lands it in both the server and the gate automatically — no
    /// hardcoded `read_router()`+`mutate_router()` list to fall out of sync.
    pub(crate) fn routers() -> Vec<ToolRouter<Self>> {
        vec![Self::read_router(), Self::mutate_router()]
    }

    /// Shared execution core for EVERY tool handler under the in-process
    /// serialization lock (NRN-55): acquire `call_lock` for the full duration of
    /// the handler's vault work, run the sync body on a `spawn_blocking` thread
    /// after the per-request seam, then map its `anyhow::Result` into the rmcp
    /// result. The handler produces its OWN `IntoCallToolResult` wrapper `R` —
    /// `Json<T>` for a plain read, or `MutationResult<T>` for a tool that sets
    /// `isError` — so this core imposes no envelope. [`run_tool`](Self::run_tool)
    /// builds on it for read tools; the mutation tools and `vault.get` call it
    /// directly to return their own wrapper (`MutationResult`, NRN-214).
    ///
    /// The lock is acquired FIRST (async), then the sync vault work runs on a
    /// `spawn_blocking` thread rather than inline on the async worker. Under the
    /// warm host daemon (`norn serve`) many connections share one runtime; a
    /// long-running query executed inline would occupy a worker thread and could
    /// starve the O(1) control-ping path (ADR 0005 requires pings answer
    /// promptly regardless of query load). Running the SQLite work off the async
    /// workers keeps them free for accepts, pings, and other vaults. The NRN-55
    /// serialization guarantee is unchanged: `call_lock` is still held across the
    /// whole blocking call, so per-vault work stays single-flight.
    async fn run_wrapped<R, F>(&self, tool: &'static str, f: F) -> Result<Noted<R>, rmcp::ErrorData>
    where
        R: Send + 'static,
        F: FnOnce(&VaultContext, &RequestScope) -> anyhow::Result<R> + Send + 'static,
    {
        let _guard = self.call_lock.lock().await;
        let ctx = Arc::clone(&self.ctx);
        let emit_serve_marker = self.emit_serve_markers;
        // The per-request seam (`begin_request`) runs under `call_lock`, off the
        // async workers, before the tool body — so every tool (including the ones
        // that bypass `query_cache` and go straight to `load_graph_index`) gets
        // root-liveness + a fresh, request-stable config each call (FIX-1). It
        // returns the request's `RequestScope` (NRN-253) — a fresh note buffer and
        // bound config, private to this request — which the tool body threads and
        // which is drained back here. The whole per-request lifecycle (create →
        // run → attribute-error → drain) runs on the ONE blocking thread, so the
        // scope never crosses the `.await` and needs no lifetime beyond the body.
        let joined = tokio::task::spawn_blocking(move || -> (Vec<String>, anyhow::Result<R>) {
            let scope = match ctx.begin_request() {
                Ok(scope) => scope,
                // A begin_request failure (RootGone / config parse error) produced
                // no scope and no notes; it is never a corruption-class error, so
                // there is nothing to attribute or drain.
                Err(err) => return (Vec::new(), Err(err)),
            };
            // Per-call served marker (NRN-94 review F6; NRN-222 review):
            // daemon-only (`new_daemon` sets the flag), so a stdio `norn mcp`
            // process writes nothing. Emitted HERE — after the per-request seam
            // succeeds, immediately before the handler — so "served" means the
            // handler actually ran (a `begin_request` failure logs nothing, and
            // the routing proofs' exact counts never overcount), and the
            // possibly-blocking stderr write happens on this blocking thread,
            // never on an async worker (ADR 0005: a wedged stderr consumer must
            // not park the workers that answer control pings).
            if emit_serve_marker {
                eprintln!("norn serve: served {tool}");
            }
            let result = f(&ctx, &scope);
            // Attribute a corruption-class SQLite failure to the generation THIS
            // request bound (carried by the scope) so the next request fully
            // reopens (integrity_check → rebuild) — the warm-mode self-heal for
            // in-place corruption (FIX-3). No-op in cold mode / non-corruption.
            // Done here, while the scope is still alive, keying the floor bump off
            // the request's bound generation rather than whatever is `current`.
            if let Err(err) = &result {
                ctx.note_tool_error(&scope, err);
            }
            // Drain THIS request's notes off its own scope; a fresh scope per
            // request is what bounds every note to the request that produced it,
            // with no shared buffer to leak across concurrent connections
            // (NRN-215 / NRN-253). On the error path the notes are dropped (a bare
            // JSON-RPC error carries no structuredContent for a note to ride — the
            // capture point already wrote each to the daemon's stderr, and a routed
            // client re-produces them via a verified Direct run).
            (scope.take_operator_notes(), result)
        })
        .await;
        match joined {
            // A tool-level FAILURE flagged in-band — the NRN-219/220 refusal shape,
            // `MutationResult { is_error: true }`, and vault.get's semantic
            // not-found — flows through THIS arm too (the handler returned Ok), so
            // its `isError: true` + structuredContent envelope carries the
            // request's notes exactly like a success.
            Ok((notes, Ok(value))) => Ok(Noted::new(value, notes)),
            Ok((_notes, Err(err))) => Err(to_mcp_error(err)),
            Err(join_err) => Err(rmcp::ErrorData::internal_error(
                format!("tool task failed: {join_err}"),
                None,
            )),
        }
    }

    /// Run a plain READ tool handler: the handler returns a bare payload `T`, which
    /// this wraps in `Json<T>` (rmcp auto-derives its `outputSchema`). Thin wrapper
    /// over [`run_wrapped`](Self::run_wrapped). `T: JsonSchema` is what the tool
    /// macro needs to emit the schema; `T: Serialize` is what `Json<T>` renders.
    async fn run_tool<T, F>(
        &self,
        tool: &'static str,
        f: F,
    ) -> Result<Noted<Json<T>>, rmcp::ErrorData>
    where
        T: serde::Serialize + schemars::JsonSchema + Send + 'static,
        F: FnOnce(&VaultContext, &RequestScope) -> anyhow::Result<T> + Send + 'static,
    {
        self.run_wrapped(tool, move |ctx, scope| f(ctx, scope).map(Json))
            .await
    }
}

/// The 7 READ tools — always registered. The macro
/// generates `fn read_router() -> ToolRouter<Self>` holding exactly these.
///
/// `vis = "pub(crate)"` exposes the generated constructor to the crate so the
/// CLI↔MCP parity gate (`super::parity_gate`) can enumerate the exact tool
/// schemas the server serves via `ToolRouter::list_all()` — the same seam
/// `tools/list` uses, so the parity test cannot drift from the live surface.
#[tool_router(router = read_router, vis = "pub(crate)")]
impl McpServer {
    /// `vault.get` — fetch one or more documents with full connection context.
    ///
    /// Thin wrapper: deserialize params, call the pure handler, map the result.
    /// All logic lives in `tools::get`; this only bridges rmcp ↔ `anyhow`. The
    /// returned [`GetOutput`] is a typed envelope whose root schema is `object`
    /// (rmcp rejects a non-object `outputSchema`); see `tools::get` for why the
    /// per-record payload stays generic JSON rather than a full `JsonSchema`
    /// derive across the core types.
    ///
    /// Unlike the other read tools, `get` returns a [`MutationResult<GetOutput>`]
    /// (via `run_wrapped`, not `run_tool`) so it can set `isError: true` when a
    /// requested target does not resolve — the same signal the CLI exits 1 on
    /// (NRN-214). It therefore publishes an explicit `output_schema` (rmcp only
    /// auto-derives for the literal `Json`).
    #[tool(
        name = "vault.get",
        description = "Fetch one or more documents: frontmatter, headings, outgoing/incoming/unresolved links, optionally body.",
        // MutationResult<T> defeats rmcp's `Json`-only auto-derive (NRN-219/214).
        output_schema = crate::mcp::mutation_result::output_schema_for::<GetOutput>()
    )]
    async fn get(
        &self,
        Parameters(p): Parameters<crate::mcp::tools::get::GetParams>,
    ) -> Result<Noted<crate::mcp::mutation_result::MutationResult<GetOutput>>, rmcp::ErrorData>
    {
        self.run_wrapped(tool_names::GET, |ctx, scope| {
            crate::mcp::tools::get::handle_output(ctx, scope, p)
        })
        .await
    }

    /// `vault.audit` — read the mutation audit trail (event stream).
    ///
    /// Thin wrapper: deserialize params, call the pure handler, map the result.
    /// All logic lives in `tools::audit`, which builds a `Filter` from the params,
    /// resolves the events dir via `cache::events_dir_for`, and calls `read_events`.
    /// Read-only: it never writes files or mutates the vault.
    #[tool(
        name = "vault.audit",
        description = "Read the vault mutation audit trail (event stream): recent mutations with status/target/trace, newest-first and filterable. Read-only.",
        // The `Noted<Json<T>>` envelope defeats rmcp's `Json`-only auto-derive, so
        // publish the payload schema explicitly (same schema `Json<AuditOutput>`
        // derived before — NRN-215).
        output_schema = crate::mcp::mutation_result::output_schema_for::<AuditOutput>()
    )]
    async fn audit(
        &self,
        Parameters(p): Parameters<crate::mcp::tools::audit::AuditParams>,
    ) -> Result<Noted<Json<AuditOutput>>, rmcp::ErrorData> {
        self.run_tool(tool_names::AUDIT, |ctx, scope| {
            crate::mcp::tools::audit::handle_output(ctx, scope, p)
        })
        .await
    }

    /// `vault.count` — count documents in the vault, total or grouped.
    ///
    /// Thin wrapper: deserialize params, call the pure handler, map the result.
    /// All logic lives in `tools::count`; this only bridges rmcp ↔ `anyhow`. The
    /// returned [`CountEnvelope`] is a typed flat object whose root schema is
    /// `type: object` (rmcp rejects non-object `outputSchema`). See `tools::count`
    /// for why `CountOutput`'s untagged enum is projected into the envelope.
    #[tool(
        name = "vault.count",
        description = "Count documents in the vault — total, or grouped by a frontmatter field — with the find filter surface.",
        // `Noted<Json<T>>` envelope — publish the payload schema explicitly (NRN-215).
        output_schema = crate::mcp::mutation_result::output_schema_for::<CountEnvelope>()
    )]
    async fn count(
        &self,
        Parameters(p): Parameters<crate::mcp::tools::count::CountParams>,
    ) -> Result<Noted<Json<CountEnvelope>>, rmcp::ErrorData> {
        self.run_tool(tool_names::COUNT, |ctx, scope| {
            crate::mcp::tools::count::handle(ctx, scope, p)
        })
        .await
    }

    /// `vault.find` — full-text + metadata document search.
    ///
    /// Thin wrapper: deserialize params, call the pure handler, map the result.
    /// All logic lives in `tools::find`, which runs the shared `find::query`
    /// seam (the same selection/JSON path behind `norn find --format json`), so
    /// the MCP tool and the CLI can't drift on filtering, sort, limit, or `--col`.
    /// The returned [`FindOutput`] is a typed envelope with a `type: object` root
    /// (rmcp rejects a non-object `outputSchema`); the per-document payload stays
    /// generic JSON, matching the `vault.get` shape.
    #[tool(
        name = "vault.find",
        description = "Find documents in the vault — full-text + metadata filters with sort, limit, and paging.",
        // `Noted<Json<T>>` envelope — publish the payload schema explicitly (NRN-215).
        output_schema = crate::mcp::mutation_result::output_schema_for::<FindOutput>()
    )]
    async fn find(
        &self,
        Parameters(p): Parameters<crate::mcp::tools::find::FindParams>,
    ) -> Result<Noted<Json<FindOutput>>, rmcp::ErrorData> {
        self.run_tool(tool_names::FIND, |ctx, scope| {
            crate::mcp::tools::find::handle(ctx, scope, p)
        })
        .await
    }

    /// `vault.validate` — validate vault graph facts and configured frontmatter/link rules.
    ///
    /// Thin wrapper: deserialize params, call the pure handler, map the result.
    /// All logic lives in `tools::validate`, which drives the same pipeline as
    /// `norn validate` (cache → graph index → `validate_with_compiled` →
    /// `filter_findings`) and returns findings as serialized JSON values in the
    /// [`ValidateOutput`] envelope. The envelope root is `type: object` (rmcp
    /// rejects a non-object `outputSchema`); per-finding payload stays generic
    /// JSON because `Finding` carries `Utf8PathBuf` which has no `JsonSchema` impl.
    #[tool(
        name = "vault.validate",
        description = "Validate vault graph facts and configured frontmatter/link rules; returns structured findings.",
        // `Noted<Json<T>>` envelope — publish the payload schema explicitly (NRN-215).
        output_schema = crate::mcp::mutation_result::output_schema_for::<ValidateOutput>()
    )]
    async fn validate(
        &self,
        Parameters(p): Parameters<crate::mcp::tools::validate::ValidateParams>,
    ) -> Result<Noted<Json<ValidateOutput>>, rmcp::ErrorData> {
        self.run_tool(tool_names::VALIDATE, |ctx, scope| {
            crate::mcp::tools::validate::handle(ctx, scope, p)
        })
        .await
    }

    /// `vault.repair` — produce a deterministic MigrationPlan without applying it.
    ///
    /// Thin wrapper: deserialize params, call the pure handler, map the result.
    /// All logic lives in `tools::repair`, which drives the same pipeline as
    /// `norn repair --plan` (cache → graph index → findings → `plan_from_findings`)
    /// and returns the in-memory `MigrationPlan` serialized as `serde_json::Value`
    /// in the [`RepairOutput`] envelope. The plan JSON is identical to what
    /// `norn repair --plan --format json` emits — `vault.apply` (Task 12)
    /// can consume it unchanged. The tool is READ-ONLY: it never writes files,
    /// never calls the applier, and never mutates the vault.
    #[tool(
        name = "vault.repair",
        description = "Produce a deterministic repair MigrationPlan (closest-match link rewrites, frontmatter fixes) without applying it. Feed the plan to vault.apply to execute.",
        // `Noted<Json<T>>` envelope — publish the payload schema explicitly (NRN-215).
        output_schema = crate::mcp::mutation_result::output_schema_for::<RepairOutput>()
    )]
    async fn repair(
        &self,
        Parameters(p): Parameters<crate::mcp::tools::repair::RepairParams>,
    ) -> Result<Noted<Json<RepairOutput>>, rmcp::ErrorData> {
        self.run_tool(tool_names::REPAIR, |ctx, scope| {
            crate::mcp::tools::repair::handle(ctx, scope, p)
        })
        .await
    }

    /// `vault.describe` — describe this vault for an off-filesystem client.
    ///
    /// Thin wrapper: deserialize params, call the pure handler, map the result.
    /// All logic lives in `tools::describe`, which assembles the folder tree
    /// (from a paths query), the declared path rules, and the frontmatter schema
    /// from `ctx.config`, plus (when `data: true` or `by` is set) a contents-summary
    /// via `crate::describe::describe` over the find-filter surface. The returned
    /// [`DescribeOutput`] derives `JsonSchema` directly (its fields are
    /// `Vec<String>` + structs of `String`/`Value`), so no Value-only envelope is
    /// needed; the root is still `type: object`. Read-only: it never opens the
    /// vault for mutation.
    #[tool(
        name = "vault.describe",
        description = "Describe this vault for an off-filesystem client: the folder tree, the declared path rules (which glob gets which frontmatter defaults — i.e. where each kind of doc lives), the frontmatter schema (field types, allowed values, required fields), and — with data: true (or by set) — a contents-summary (totals, field distributions, date bounds) filtered by the same predicates as vault.find/vault.count. Use it to construct the correct path for a new document, then call vault.new.",
        // `Noted<Json<T>>` envelope — publish the payload schema explicitly (NRN-215).
        output_schema = crate::mcp::mutation_result::output_schema_for::<DescribeOutput>()
    )]
    async fn describe(
        &self,
        Parameters(p): Parameters<crate::mcp::tools::describe::DescribeParams>,
    ) -> Result<Noted<Json<DescribeOutput>>, rmcp::ErrorData> {
        self.run_tool(tool_names::DESCRIBE, move |ctx, scope| {
            crate::mcp::tools::describe::handle(ctx, scope, &p)
        })
        .await
    }
}

/// The 7 MUTATION tools. The macro generates `fn mutate_router() -> ToolRouter<Self>`
/// holding exactly these; `new` merges it into the stored router alongside
/// `read_router` (see `routers`).
///
/// `vis = "pub(crate)"` — see `read_router` above — lets the parity gate
/// enumerate the mutation-tool schemas too.
#[tool_router(router = mutate_router, vis = "pub(crate)")]
impl McpServer {
    /// `vault.new` — create a new document with schema-scaffolded frontmatter.
    ///
    /// Thin wrapper: deserialize params, call the pure handler, map the result.
    /// All logic lives in `tools::new`, which mirrors the CLI `norn new` non-TTY
    /// path: preflight → `build_plan` → DRY-RUN unless `confirm` → on confirm
    /// acquire the per-vault mutation lock, open the event sink, apply via the
    /// shared `repair_apply::apply_repair_plan_with_context` with a single
    /// `create_document` change, and return the JSON envelope. The mutation-safety
    /// contract (`confirm:false` = plan-only, no file created; `confirm:true` =
    /// file created, audited) is the same as `vault.set`.
    #[tool(
        name = "vault.new",
        description = "Create a new document with schema-scaffolded frontmatter from its path. DRY-RUN by default (returns the planned creation without writing); pass confirm:true to create the file.",
        // Explicit schema — MutationResult<T> defeats rmcp's `Json`-only auto-derive (NRN-219/220).
        output_schema = crate::mcp::mutation_result::output_schema_for::<NewOutput>()
    )]
    async fn new_document(
        &self,
        Parameters(p): Parameters<crate::mcp::tools::new::NewParams>,
    ) -> Result<Noted<MutationResult<NewOutput>>, rmcp::ErrorData> {
        // A coded preflight refusal (`destination-exists`, containment, …) crosses
        // as a structured `refused` report + `isError:true` (NRN-220); other
        // failures still propagate as a bare MCP `Err`.
        self.run_wrapped(tool_names::NEW, |ctx, scope| {
            crate::mcp::tools::new::handle_output(ctx, scope, p)
        })
        .await
    }

    /// `vault.set` — the first MCP mutation tool; establishes the
    /// mutation-safety contract (default dry-run; `confirm:true` writes).
    ///
    /// Thin wrapper: deserialize params, call the pure handler, map the result.
    /// All logic lives in `tools::set`, which mirrors `norn set`'s dispatch
    /// (`preflight_and_plan` → DRY-RUN unless `confirm` → on confirm acquire the
    /// per-vault mutation lock and apply via the shared repair applier). The
    /// returned [`SetOutput`] is a typed envelope with a `type: object` root
    /// (rmcp rejects a non-object `outputSchema`); the `SetReport` payload stays
    /// generic JSON because it carries a `Utf8PathBuf` with no `JsonSchema` impl.
    /// This handler funnels through `run_wrapped` like every other tool, so the
    /// process-wide `call_lock` serializes it; the per-vault mutation lock it
    /// acquires inside `handle` (confirm path only) is a different, inner lock.
    #[tool(
        name = "vault.set",
        description = "Update one document's frontmatter (and optionally replace its body), schema-aware. DRY-RUN by default — returns the planned change without writing. Pass confirm:true to apply.",
        // Explicit schema — MutationResult<T> defeats rmcp's `Json`-only auto-derive (NRN-219/220).
        output_schema = crate::mcp::mutation_result::output_schema_for::<SetOutput>()
    )]
    async fn set(
        &self,
        Parameters(p): Parameters<crate::mcp::tools::set::SetParams>,
    ) -> Result<Noted<MutationResult<SetOutput>>, rmcp::ErrorData> {
        // A coded refusal — a precondition/CAS failure (NRN-220) or a schema/
        // argument refusal (`value-not-allowed`, `required-field-removed`, …;
        // NRN-221) — crosses as a structured `refused` report + `isError:true`;
        // genuinely internal errors still propagate as a bare MCP `Err`.
        self.run_wrapped(tool_names::SET, |ctx, scope| {
            crate::mcp::tools::set::handle_output(ctx, scope, p)
        })
        .await
    }

    /// `vault.edit` — sub-document partial edits (str_replace + structural
    /// section ops). DRY-RUN by default; `confirm:true` applies. Funnels through
    /// `run_wrapped` like every tool (process-wide `call_lock`).
    ///
    /// Thin wrapper: deserialize params, call the pure handler, map the result.
    /// All logic lives in `tools::edit`, which mirrors `norn edit`'s dispatch via
    /// the shared `edit::synth` preflight; the returned [`EditOutput`] is the same
    /// typed-envelope shape as [`SetOutput`] (a `type: object` root wrapping the
    /// `EditReport` as generic JSON, since the report carries a `Utf8PathBuf`).
    #[tool(
        name = "vault.edit",
        description = "Edit one document's body with atomic content-anchored partial edits (str_replace + section ops). DRY-RUN by default — returns the plan without writing. Pass confirm:true to apply.",
        // Explicit schema — MutationResult<T> defeats rmcp's `Json`-only auto-derive (NRN-219/220).
        output_schema = crate::mcp::mutation_result::output_schema_for::<EditOutput>()
    )]
    async fn edit(
        &self,
        Parameters(p): Parameters<crate::mcp::tools::edit::EditParams>,
    ) -> Result<Noted<MutationResult<EditOutput>>, rmcp::ErrorData> {
        // A coded refusal — `expected_hash` CAS drift or an anchor miss
        // (`anchor-not-found`, …) — crosses as a structured `refused` report +
        // `isError:true` (NRN-220); other errors still propagate as a bare `Err`.
        self.run_wrapped(tool_names::EDIT, |ctx, scope| {
            crate::mcp::tools::edit::handle_output(ctx, scope, p)
        })
        .await
    }

    /// `vault.move` — move/rename a document, cascading backlink rewrites.
    ///
    /// Thin wrapper: deserialize params, call the pure handler, map the result.
    /// All logic lives in `tools::move_doc`, which mirrors the CLI `norn move`
    /// non-TTY path: preflight → one-op `MigrationPlan` → DRY-RUN unless `confirm`
    /// → on confirm acquire the per-vault mutation lock, open the event sink, and
    /// apply via the shared `applier::apply_migration_plan` (which cascades the
    /// backlink rewrites). Same mutation-safety + audit contract as `vault.set`.
    #[tool(
        name = "vault.move",
        description = "Move/rename a document, cascading backlink rewrites across the vault. DRY-RUN by default; confirm:true to apply.",
        // MutationResult<T> is not the literal `Json`, so rmcp cannot auto-derive
        // the schema — publish it explicitly (NRN-219).
        output_schema = crate::mcp::mutation_result::output_schema_for::<MoveOutput>()
    )]
    async fn move_doc(
        &self,
        Parameters(p): Parameters<crate::mcp::tools::move_doc::MoveParams>,
    ) -> Result<Noted<MutationResult<MoveOutput>>, rmcp::ErrorData> {
        self.run_wrapped(tool_names::MOVE, |ctx, scope| {
            crate::mcp::tools::move_doc::handle_output(ctx, scope, p)
        })
        .await
    }

    /// `vault.delete` — delete a document, optionally redirecting incoming links.
    ///
    /// Thin wrapper: deserialize params, call the pure handler, map the result.
    /// All logic lives in `tools::delete`, which mirrors the CLI `norn delete`
    /// non-TTY path: preflight (backlink-policy refusal) → one-op
    /// `delete_document` `MigrationPlan` → DRY-RUN unless `confirm` → on confirm
    /// acquire the per-vault mutation lock, open the event sink, and apply via the
    /// shared `applier::apply_migration_plan` (deleting + optionally redirecting
    /// incoming links). DESTRUCTIVE: the `confirm:false` dry-run removes nothing.
    #[tool(
        name = "vault.delete",
        description = "Delete a document, optionally redirecting incoming links to an alternate target. DRY-RUN by default; confirm:true to apply.",
        // Explicit schema — MutationResult<T> defeats rmcp's `Json`-only auto-derive (NRN-219).
        output_schema = crate::mcp::mutation_result::output_schema_for::<DeleteOutput>()
    )]
    async fn delete(
        &self,
        Parameters(p): Parameters<crate::mcp::tools::delete::DeleteParams>,
    ) -> Result<Noted<MutationResult<DeleteOutput>>, rmcp::ErrorData> {
        self.run_wrapped(tool_names::DELETE, |ctx, scope| {
            crate::mcp::tools::delete::handle_output(ctx, scope, p)
        })
        .await
    }

    /// `vault.rewrite_wikilink` — retarget a wikilink across the vault, no move.
    ///
    /// Thin wrapper: deserialize params, call the pure handler, map the result.
    /// All logic lives in `tools::rewrite_wikilink`, which mirrors the CLI
    /// `norn rewrite-wikilink` non-TTY path: one-op `rewrite_wikilink`
    /// `MigrationPlan` → DRY-RUN unless `confirm` → on confirm acquire the
    /// per-vault mutation lock, open the event sink, and apply via the shared
    /// `applier::apply_migration_plan` (whose planner fans the op out into
    /// per-file body + frontmatter rewrites). No file is moved.
    #[tool(
        name = "vault.rewrite_wikilink",
        description = "Rewrite all occurrences of a wikilink target across the vault (body + frontmatter), without moving any file. DRY-RUN by default; confirm:true to apply.",
        // Explicit schema — MutationResult<T> defeats rmcp's `Json`-only auto-derive (NRN-219).
        output_schema = crate::mcp::mutation_result::output_schema_for::<RewriteWikilinkOutput>()
    )]
    async fn rewrite_wikilink(
        &self,
        Parameters(p): Parameters<crate::mcp::tools::rewrite_wikilink::RewriteWikilinkParams>,
    ) -> Result<Noted<MutationResult<RewriteWikilinkOutput>>, rmcp::ErrorData> {
        self.run_wrapped(tool_names::REWRITE_WIKILINK, |ctx, scope| {
            crate::mcp::tools::rewrite_wikilink::handle_output(ctx, scope, p)
        })
        .await
    }

    /// `vault.apply` — apply a `MigrationPlan` (e.g. from `vault.repair`).
    ///
    /// Thin wrapper: deserialize params, call the pure handler, map the result.
    /// All logic lives in `tools::apply`, which mirrors `norn apply`'s
    /// non-TTY path: validate `schema_version` → DRY-RUN unless `confirm` → on
    /// confirm acquire the per-vault mutation lock, open the event sink, and apply
    /// via the shared `applier::apply_migration_plan`. The plan is accepted inline
    /// (as a `serde_json::Value`), so callers can pipe `vault.repair`'s
    /// `result.structuredContent.plan` directly here without writing to a file.
    /// Same mutation-safety + audit contract as `vault.move` / `vault.delete`.
    #[tool(
        name = "vault.apply",
        description = "Apply a MigrationPlan (e.g. from vault.repair) to the vault — moves, deletes, link rewrites, frontmatter ops. DRY-RUN by default (forecasts the apply); pass confirm:true to execute.",
        // Explicit schema — MutationResult<T> defeats rmcp's `Json`-only auto-derive (NRN-219).
        output_schema = crate::mcp::mutation_result::output_schema_for::<ApplyOutput>()
    )]
    async fn apply(
        &self,
        Parameters(p): Parameters<crate::mcp::tools::apply::ApplyParams>,
    ) -> Result<Noted<MutationResult<ApplyOutput>>, rmcp::ErrorData> {
        self.run_wrapped(tool_names::APPLY, |ctx, scope| {
            crate::mcp::tools::apply::handle_output(ctx, scope, p)
        })
        .await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        // `ServerInfo` (alias for `InitializeResult`) is `#[non_exhaustive]` in
        // rmcp, so the struct-literal form does not compile — start from `Default`
        // and override the fields we care about.
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        // NRN-187: identify as "norn" at the crate version. `Default` inherits the
        // rmcp crate's own build env (name="rmcp"), so a client's `initialize`
        // response would otherwise advertise the transport library, not this
        // server. Set it explicitly so `serverInfo.name`/`.version` name norn.
        info.server_info = Implementation::new("norn", env!("CARGO_PKG_VERSION"));
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use rmcp::handler::server::wrapper::Parameters;
    use tempfile::TempDir;

    /// Drift guard for the served-marker names (NRN-222 review): the rmcp
    /// `#[tool]` macro only accepts a string literal for `name`, so the marker
    /// consts in [`tool_names`] cannot be shared with the attributes directly.
    /// This pins the two by construction: the const table must set-equal the
    /// ADVERTISED catalog (`routers()` → `list_all()`, the same seam
    /// `tools/list` serves) — a marker const that drifts from its attribute, or
    /// a new tool missing from the table, fails here deterministically.
    #[test]
    fn served_marker_names_match_the_advertised_catalog() {
        let mut catalog: Vec<String> = McpServer::routers()
            .iter()
            .flat_map(|router| router.list_all())
            .map(|tool| tool.name.to_string())
            .collect();
        catalog.sort();
        let mut names: Vec<String> = tool_names::ALL.iter().map(|s| s.to_string()).collect();
        names.sort();
        assert_eq!(
            names, catalog,
            "tool_names::ALL (the served-marker consts) must set-equal the \
             advertised tool catalog"
        );
    }

    /// Seed a temp vault with several docs and NO pre-built cache. Cold start is
    /// the point: the race window is `Cache::open_with_config`'s
    /// inspect/DDL/recreate sequence, which only runs the first time the cache is
    /// opened. Returning the `TempDir` keeps the vault alive for the test.
    ///
    /// We deliberately do NOT set `XDG_CACHE_HOME` here: `std::env::set_var` is
    /// process-global and races other in-binary tests that read the default cache
    /// dir. Cache identity is keyed by a hash of the (unique) vault root, so the
    /// fresh tempdir already guarantees a cold, isolated cache under the default
    /// `~/.cache/norn/<hash>/` — same approach the `context.rs` unit tests use.
    fn cold_seeded_vault() -> (TempDir, Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-concurrency-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        for (name, kind) in [
            ("alpha", "note"),
            ("beta", "task"),
            ("gamma", "log"),
            ("delta", "note"),
            ("epsilon", "task"),
        ] {
            std::fs::write(
                root.join(format!("{name}.md")),
                format!("---\ntype: {kind}\nstatus: active\n---\n{name} body\n"),
            )
            .unwrap();
        }
        (tmp, root)
    }

    /// NRN-55 regression: N concurrent cold-start tool calls must all succeed.
    ///
    /// Without the `call_lock`, two worker threads hitting `vault.get` at the same
    /// time race `Cache::open_with_config`'s cold-start DDL/recreate window
    /// (upstream of the flock `WriteLock`, guarded only by SQLite's busy_timeout),
    /// and ≥1 call intermittently fails with "database is locked". With the lock,
    /// the cold-start cache open is serialized and every call succeeds
    /// deterministically.
    ///
    /// Verified to have teeth: with the `_guard` line removed from `run_tool`
    /// (pre-fix behavior), this test fails/flakes with "database is locked"; with
    /// the lock in place it passes on every run.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_cold_start_calls_all_succeed() {
        let (_tmp, root) = cold_seeded_vault();
        let ctx = Arc::new(VaultContext::open(&root, None).expect("VaultContext::open"));
        let server = McpServer::new(ctx);

        const N: usize = 8;
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..N {
            let server = server.clone();
            set.spawn(async move {
                server
                    .get(Parameters(crate::mcp::tools::get::GetParams {
                        targets: vec!["alpha".to_string()],
                        col: None,
                        ..Default::default()
                    }))
                    .await
            });
        }

        let mut results = Vec::with_capacity(N);
        while let Some(joined) = set.join_next().await {
            results.push(joined.expect("tool-call task should not panic"));
        }

        let failures: Vec<String> = results
            .iter()
            .filter_map(|r| r.as_ref().err().map(|e| format!("{e:?}")))
            .collect();
        assert!(
            failures.is_empty(),
            "all {N} concurrent cold-start vault.get calls must succeed; \
             {} failed: {failures:?}",
            failures.len()
        );

        // Sanity: each successful call returned the seeded `alpha` record.
        for r in &results {
            let out = r.as_ref().expect("call should be Ok");
            assert_eq!(
                out.inner().value().records.len(),
                1,
                "vault.get for `alpha` should return exactly one record"
            );
        }
    }

    /// FIX-1 (NRN-93): a tool that bypasses `query_cache` (validate goes straight
    /// to `load_graph_index`) must still observe a config change between two warm
    /// requests. The per-request `begin_request` seam refreshes config before
    /// every tool body; without it, warm-mode config for these tools goes stale
    /// for the daemon's lifetime and routed results diverge from a direct CLI run.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn warm_validate_reflects_config_change_across_requests() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-warm-cfg-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        // A broken wikilink inside templates/ — validate flags it until a later
        // files.ignore rule hides it.
        let templates = root.join("templates");
        std::fs::create_dir_all(templates.as_std_path()).unwrap();
        std::fs::write(
            templates.join("tpl.md"),
            "---\ntype: note\ntitle: T\n---\n\nSee [[MissingTarget]].\n",
        )
        .unwrap();

        let ctx = Arc::new(VaultContext::open_warm(&root).expect("open_warm"));
        let server = McpServer::new(ctx);

        let out1 = server
            .validate(Parameters(
                crate::mcp::tools::validate::ValidateParams::default(),
            ))
            .await
            .expect("first validate");
        assert!(
            out1.inner()
                .0
                .findings
                .as_ref()
                .is_some_and(|f| !f.is_empty()),
            "baseline: broken wikilink must produce a finding"
        );

        // Add files.ignore for templates/** AFTER the warm context opened.
        let norn_dir = root.join(".norn");
        std::fs::create_dir_all(norn_dir.as_std_path()).unwrap();
        std::fs::write(
            norn_dir.join("config.yaml"),
            "files:\n  ignore:\n    - \"templates/**\"\n",
        )
        .unwrap();

        let out2 = server
            .validate(Parameters(
                crate::mcp::tools::validate::ValidateParams::default(),
            ))
            .await
            .expect("second validate");
        assert!(
            out2.inner()
                .0
                .findings
                .as_ref()
                .is_some_and(|f| f.is_empty()),
            "config change (files.ignore) must be visible to the next warm request; got {:?}",
            out2.inner().0.findings
        );
    }

    /// The `run_wrapped` funnel forwards a request's operator notes in the tool
    /// envelope (NRN-215): a note recorded on the context while the body runs
    /// comes back paired with the result and serializes as the `operator_notes`
    /// sibling in `structuredContent`; a note-free request adds nothing. The
    /// note is recorded here through the same `push_operator_note` seam the real
    /// capture point (`query_cache_warm`'s lock-timeout arm) uses; the GENUINE
    /// end-to-end trigger — a live daemon under real flock contention, note
    /// re-emitted byte-identically on the routed CLI's stderr — is proven by
    /// `tests/serve_note_forwarding.rs`, where the daemon child process owns its
    /// own (short-lock-timeout) environment.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_wrapped_forwards_request_notes_in_the_envelope() {
        use rmcp::handler::server::tool::IntoCallToolResult;

        let (_tmp, root) = cold_seeded_vault();
        let ctx = Arc::new(VaultContext::open_warm(&root).expect("open_warm"));
        let server = McpServer::new(ctx);

        // A note-free request forwards nothing.
        let quiet = server
            .validate(Parameters(
                crate::mcp::tools::validate::ValidateParams::default(),
            ))
            .await
            .expect("validate");
        assert!(
            quiet.notes().is_empty(),
            "a note-free request must forward no operator note"
        );

        // A request that records a note gets it back in the envelope. The body
        // returns an object payload, like every real tool (`Noted` injects the
        // sibling only into an object-shaped structuredContent).
        let noted = server
            .run_wrapped(
                "vault.count",
                |_ctx, scope| -> anyhow::Result<Json<serde_json::Value>> {
                    scope.push_operator_note(crate::cache::LOCK_CONTENTION_NOTE);
                    Ok(Json(serde_json::json!({ "total": 0 })))
                },
            )
            .await
            .expect("noted request");
        assert_eq!(
            noted.notes(),
            [crate::cache::LOCK_CONTENTION_NOTE],
            "the request's note must ride its own result"
        );
        let result = noted.into_call_tool_result().expect("serialize");
        let sc = result
            .structured_content
            .expect("structured content present");
        assert_eq!(
            sc[crate::mcp::notes::OPERATOR_NOTES_KEY],
            serde_json::json!([crate::cache::LOCK_CONTENTION_NOTE]),
            "structuredContent.operator_notes must carry the forwarded note"
        );
    }

    /// Cross-request note isolation is now STRUCTURAL (NRN-253): every request owns
    /// a fresh `RequestScope` note buffer that `run_wrapped` drains, so a note
    /// recorded by a request that then FAILED (the bare-Err arm, whose JSON-RPC
    /// error carries no structuredContent for the note to ride) lives and dies with
    /// that request's scope and can never leak into the NEXT request's envelope —
    /// there is no shared context buffer to leak through, regardless of how the
    /// prior request ended. (The failed request's note itself is not lost: the
    /// capture point writes every note to the daemon's stderr as it is recorded,
    /// and a routed client maps a JSON-RPC error to a verified Direct run, which
    /// re-produces the note canonically.) This replaces the pre-NRN-253
    /// `begin_request_clears_notes_left_by_a_failed_request` test, whose
    /// leftover-note-clearing precondition per-request buffers make structurally
    /// impossible.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_request_notes_do_not_leak_into_the_next_request() {
        let (_tmp, root) = cold_seeded_vault();
        let ctx = Arc::new(VaultContext::open_warm(&root).expect("open_warm"));
        let server = McpServer::new(ctx);

        // A request records a note into its OWN scope, then its body fails: the
        // note has no envelope to ride and is dropped with that request's scope.
        let failed = server
            .run_wrapped("vault.count", |_ctx, scope| -> anyhow::Result<Json<()>> {
                scope.push_operator_note(crate::cache::LOCK_CONTENTION_NOTE);
                anyhow::bail!("boom after the note")
            })
            .await;
        assert!(failed.is_err(), "the failing body must surface as Err");

        // The NEXT request runs on a FRESH scope — no stale note leaks in.
        let next = server
            .validate(Parameters(
                crate::mcp::tools::validate::ValidateParams::default(),
            ))
            .await
            .expect("next validate");
        assert!(
            next.notes().is_empty(),
            "a note left by a failed request must not leak into the next \
             request's envelope, got {:?}",
            next.notes()
        );
    }

    /// NRN-187: `get_info` must advertise this server as "norn" at the crate
    /// version — not rmcp's build-env default (name="rmcp"). This is the payload
    /// a client reads out of the `initialize` response's `serverInfo`.
    #[test]
    fn get_info_advertises_norn_server_identity() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-serverinfo-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let ctx = Arc::new(VaultContext::open(&root, None).expect("VaultContext::open"));
        let server = McpServer::new(ctx);

        let info = server.get_info();
        assert_eq!(
            info.server_info.name, "norn",
            "serverInfo.name must be 'norn', not rmcp's build-env default"
        );
        assert_eq!(
            info.server_info.version,
            env!("CARGO_PKG_VERSION"),
            "serverInfo.version must be norn's crate version"
        );
    }
}
