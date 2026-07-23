//! MCP tool surfaces — one module per tool.
//!
//! Each module owns two things: the tool's MCP-facing **param struct** (a
//! `schemars::JsonSchema` deriver whose generated schema IS the `tools/list`
//! `inputSchema` — the published MCP contract) and, where the MCP
//! output envelope differs from the verb's raw
//! wire `Report`, the **output struct** plus the pure `to_wire` / `envelope`
//! mappers. The `#[tool]` methods themselves live on `McpServer` in `server.rs`
//! (rmcp requires them on the router impl block); each is a thin wrapper that
//! builds the wire `Params` here, runs the routed owner request, and maps the
//! `Report` back through here.
//!
//! # Flat reads, wrapped mutations (deliberate, standing contract)
//!
//! `structuredContent` has exactly two shapes across the whole tool surface,
//! by verb family — this is the deliberate contract, not an accident of
//! per-tool authoring: every READ tool (`count` / `find` / `get` / `describe`
//! / `validate` / `repair`) projects its report FLAT — the report's own
//! fields sit at the top level of `structuredContent`. Every MUTATION tool
//! (`set` / `new` / `edit` / `move` / `delete` / `rewrite_wikilink` / `apply`)
//! wraps its report under one `{ report: … }` envelope key. The split tracks
//! [`crate::mutation_result::MutationResult`] vs a bare `Json<T>` return: a
//! mutation report's `outcome` (see the preview-detection contract on each
//! mutator's tool description) is a per-verb-family authoritative fact a
//! client keys `isError` on, so it stays inside a stable wrapper key rather
//! than competing with the report's own field names at the top level; a read
//! report has no such outcome to disambiguate, so nothing forces it out of
//! the flat shape.

pub mod apply;
pub mod count;
pub mod delete;
pub mod describe;
pub mod edit;
pub mod find;
pub mod get;
pub mod move_doc;
pub mod new;
pub mod repair;
pub mod rewrite_wikilink;
pub mod set;
pub mod validate;
