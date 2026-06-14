//! The MCP server handler.
//!
//! Task 1 is the scaffold: an empty tool router with zero `#[tool]` methods, so
//! `tools/list` answers with an empty array. Later tasks add `#[tool]` methods to
//! the `#[tool_router]` impl block below.
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
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};

use super::context::VaultContext;
use super::to_mcp_error;
use crate::mcp::tools::count::CountEnvelope;
use crate::mcp::tools::describe::DescribeOutput;
use crate::mcp::tools::find::FindOutput;
use crate::mcp::tools::get::GetOutput;
use crate::mcp::tools::new::NewOutput;
use crate::mcp::tools::repair_plan::RepairPlanOutput;
use crate::mcp::tools::set::SetOutput;
use crate::mcp::tools::validate::ValidateOutput;

#[derive(Clone)]
pub struct McpServer {
    /// Warm vault context: config held for the server lifetime; cache opened
    /// fresh per tool call via `self.ctx.query_cache()`.
    pub(crate) ctx: Arc<VaultContext>,
    /// In-process serialization lock for tool calls (NRN-55).
    ///
    /// The MCP server is one long-lived process serving many `tools/call`
    /// requests on a multi-thread tokio runtime. The tool handlers open the
    /// cache and run blocking SQLite work inline. Two concurrent calls on two
    /// worker threads can race the cold-start cache open/DDL window (upstream of
    /// the `flock`-based `WriteLock`), yielding "database is locked". The CLI is
    /// immune because it is one process per invocation; the server is not.
    ///
    /// We serialize every tool call through this async mutex so vault work runs
    /// single-flight within the process — correctness over concurrent-read
    /// throughput, which the one-vault-one-server model does not need in v1. The
    /// guard is held across the inline blocking SQLite work on purpose: that is
    /// exactly "one vault operation at a time". (`spawn_blocking` is a possible
    /// v2 optimization, deliberately out of scope here.)
    call_lock: Arc<tokio::sync::Mutex<()>>,
    tool_router: ToolRouter<Self>,
}

impl McpServer {
    pub fn new(ctx: Arc<VaultContext>) -> Self {
        Self {
            ctx,
            call_lock: Arc::new(tokio::sync::Mutex::new(())),
            tool_router: Self::tool_router(),
        }
    }

    /// Run a tool handler under the in-process serialization lock (NRN-55).
    ///
    /// Acquires `call_lock` for the full duration of the handler's vault work,
    /// then maps the `anyhow::Result` into the rmcp `Json` envelope. Every
    /// `#[tool]` method funnels through here, so the lock + the
    /// `.map(Json).map_err(to_mcp_error)` boilerplate live in exactly one place.
    ///
    /// `T: Serialize` is what `Json<T>` requires to render the result; the tool
    /// macro additionally needs `T: JsonSchema` to emit each tool's
    /// `outputSchema`. Both bounds match what the existing methods already
    /// required, so the generic does not narrow any tool's contract.
    async fn run_tool<T, F>(&self, f: F) -> Result<Json<T>, rmcp::ErrorData>
    where
        T: serde::Serialize + schemars::JsonSchema,
        F: FnOnce(&VaultContext) -> anyhow::Result<T>,
    {
        let _guard = self.call_lock.lock().await;
        f(&self.ctx).map(Json).map_err(to_mcp_error)
    }
}

#[tool_router]
impl McpServer {
    /// `vault.get` — fetch one or more documents with full connection context.
    ///
    /// Thin wrapper: deserialize params, call the pure handler, map the result.
    /// All logic lives in `tools::get`; this only bridges rmcp ↔ `anyhow`. The
    /// returned [`GetOutput`] is a typed envelope whose root schema is `object`
    /// (rmcp rejects a non-object `outputSchema`); see `tools::get` for why the
    /// per-record payload stays generic JSON rather than a full `JsonSchema`
    /// derive across the core types. Later read tools copy this thin shape.
    #[tool(
        name = "vault.get",
        description = "Fetch one or more documents: frontmatter, headings, outgoing/incoming/unresolved links, optionally body."
    )]
    async fn get(
        &self,
        Parameters(p): Parameters<crate::mcp::tools::get::GetParams>,
    ) -> Result<Json<GetOutput>, rmcp::ErrorData> {
        self.run_tool(|ctx| crate::mcp::tools::get::handle_output(ctx, p))
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
        description = "Count documents in the vault — total, or grouped by a frontmatter field — with the find filter surface."
    )]
    async fn count(
        &self,
        Parameters(p): Parameters<crate::mcp::tools::count::CountParams>,
    ) -> Result<Json<CountEnvelope>, rmcp::ErrorData> {
        self.run_tool(|ctx| crate::mcp::tools::count::handle(ctx, p))
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
        description = "Find documents in the vault — full-text + metadata filters with sort, limit, and paging."
    )]
    async fn find(
        &self,
        Parameters(p): Parameters<crate::mcp::tools::find::FindParams>,
    ) -> Result<Json<FindOutput>, rmcp::ErrorData> {
        self.run_tool(|ctx| crate::mcp::tools::find::handle(ctx, p))
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
        description = "Validate vault graph facts and configured frontmatter/link rules; returns structured findings."
    )]
    async fn validate(
        &self,
        Parameters(p): Parameters<crate::mcp::tools::validate::ValidateParams>,
    ) -> Result<Json<ValidateOutput>, rmcp::ErrorData> {
        self.run_tool(|ctx| crate::mcp::tools::validate::handle(ctx, p))
            .await
    }

    /// `vault.repair_plan` — produce a deterministic MigrationPlan without applying it.
    ///
    /// Thin wrapper: deserialize params, call the pure handler, map the result.
    /// All logic lives in `tools::repair_plan`, which drives the same pipeline as
    /// `norn repair --plan` (cache → graph index → findings → `plan_from_findings`)
    /// and returns the in-memory `MigrationPlan` serialized as `serde_json::Value`
    /// in the [`RepairPlanOutput`] envelope. The plan JSON is identical to what
    /// `norn repair --plan --format json` emits — `vault.apply_plan` (Task 12)
    /// can consume it unchanged. The tool is READ-ONLY: it never writes files,
    /// never calls the applier, and never mutates the vault.
    #[tool(
        name = "vault.repair_plan",
        description = "Produce a deterministic repair MigrationPlan (closest-match link rewrites, frontmatter fixes) without applying it. Feed the plan to vault.apply_plan to execute."
    )]
    async fn repair_plan(
        &self,
        Parameters(p): Parameters<crate::mcp::tools::repair_plan::RepairPlanParams>,
    ) -> Result<Json<RepairPlanOutput>, rmcp::ErrorData> {
        self.run_tool(|ctx| crate::mcp::tools::repair_plan::handle(ctx, p))
            .await
    }

    /// `vault.describe` — describe this vault for an off-filesystem client.
    ///
    /// Thin wrapper: deserialize params, call the pure handler, map the result.
    /// All logic lives in `tools::describe`, which assembles the folder tree
    /// (from a paths query), the declared path rules, and the frontmatter schema
    /// from `ctx.config`. The returned [`DescribeOutput`] derives `JsonSchema`
    /// directly (its fields are `Vec<String>` + structs of `String`/`Value`), so
    /// no Value-only envelope is needed; the root is still `type: object`.
    /// Read-only: it never opens the vault for mutation.
    #[tool(
        name = "vault.describe",
        description = "Describe this vault for an off-filesystem client: the folder tree, the declared path rules (which glob gets which frontmatter defaults — i.e. where each kind of doc lives), and the frontmatter schema (field types, allowed values, required fields). Use it to construct the correct path for a new document, then call vault.new."
    )]
    async fn describe(
        &self,
        Parameters(_p): Parameters<crate::mcp::tools::describe::DescribeParams>,
    ) -> Result<Json<DescribeOutput>, rmcp::ErrorData> {
        self.run_tool(crate::mcp::tools::describe::handle).await
    }

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
        description = "Create a new document with schema-scaffolded frontmatter from its path. DRY-RUN by default (returns the planned creation without writing); pass confirm:true to create the file."
    )]
    async fn new_document(
        &self,
        Parameters(p): Parameters<crate::mcp::tools::new::NewParams>,
    ) -> Result<Json<NewOutput>, rmcp::ErrorData> {
        self.run_tool(|ctx| crate::mcp::tools::new::handle_output(ctx, p))
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
    /// This handler funnels through `run_tool` like every other tool, so the
    /// process-wide `call_lock` serializes it; the per-vault mutation lock it
    /// acquires inside `handle` (confirm path only) is a different, inner lock.
    #[tool(
        name = "vault.set",
        description = "Update one document's frontmatter (and optionally replace its body), schema-aware. DRY-RUN by default — returns the planned change without writing. Pass confirm:true to apply."
    )]
    async fn set(
        &self,
        Parameters(p): Parameters<crate::mcp::tools::set::SetParams>,
    ) -> Result<Json<SetOutput>, rmcp::ErrorData> {
        self.run_tool(|ctx| crate::mcp::tools::set::handle_output(ctx, p))
            .await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        // `ServerInfo` (alias for `InitializeResult`) is `#[non_exhaustive]` in
        // rmcp 1.7.0, so the struct-literal form from the plan snippet does not
        // compile — start from `Default` and override the tools capability.
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use rmcp::handler::server::wrapper::Parameters;
    use tempfile::TempDir;

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
                out.0.records.len(),
                1,
                "vault.get for `alpha` should return exactly one record"
            );
        }
    }
}
