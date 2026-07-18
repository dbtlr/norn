//! CLI↔MCP parity forcing function (NRN-178).
//!
//! ADR 0009 makes the CLI and the MCP server **peer surfaces over one capability
//! core**: a capability reachable from one must be reachable from the other,
//! except for an explicit, justified set of carve-outs. This test converts silent
//! parity erosion into a hard build break — add a CLI flag with no MCP twin and no
//! allowlist entry, and `cargo test` fails naming the field.
//!
//! # The two seams (chosen so the test cannot drift from reality)
//!
//! * **CLI:** [`clap::CommandFactory`] → `Cli::command()`, then walk
//!   `get_subcommands()` / `get_arguments()`. This is the *same* derive-generated
//!   [`clap::Command`] the binary parses with, so the enumerated flags are exactly
//!   what the CLI accepts — no hand-maintained flag list to fall out of sync.
//! * **MCP:** [`McpServer::routers`] — the same seam `McpServer::new`
//!   builds the served router from — then
//!   [`rmcp::handler::server::router::tool::ToolRouter::list_all`]. Each returned
//!   [`rmcp::model::Tool`] carries the `input_schema` the server publishes in
//!   `tools/list`. Reading param names out of that schema means the test sees
//!   precisely what an MCP client sees — not a re-derivation that could diverge
//!   from the served surface. Consuming the shared `routers` seam (rather than a
//!   hardcoded router list) means a future third router is enumerated here for
//!   free.
//!
//! Only the **mapping** between the two is declared by hand (below). That mapping
//! *is* the parity documentation: which CLI command backs which MCP tool, which
//! flags are renamed, and which gaps are known-and-tracked.
//!
//! # Carve-out classes (ADR 0009)
//!
//! 1. **SAFETY** — MCP mutation tools are dry-run-by-default with a `confirm`
//!    param; the CLI uses a prompt plus `--yes` / `--dry-run`. So CLI `yes` and
//!    `dry_run` map to MCP `confirm`.
//! 2. **PRESENTATION** — CLI-only *rendering* knobs (`--format` on commands where
//!    it only renders an already-computed result, `--no-pager`, output-destination
//!    `--out`). Valid only when they change how output is shown, never *what* is
//!    computed. `get --format` is intentionally not in this class: `markdown`
//!    selects the exact-source representation shared with `vault.get.format`.
//! 3. **SHAPE** — CLI-only *ergonomic/surface-shape* fields with no MCP field:
//!    guards (`find --all`), aliases (`describe --stats`), mode selectors
//!    (`repair --plan`), and input file-format selection (`apply
//!    --input-format`) that MCP replaces by taking inline structured JSON. Like
//!    PRESENTATION, none compute data the other surface cannot return. (ADR 0009
//!    folds these under the presentation/naming carve-outs.)
//! 4. **NAMING MAP** — sanctioned FIELD renames: CLI `src`/`dst` positionals and
//!    `field_json`/`edits_json`/`as_rule`/… vs the MCP param names. Encoded as an
//!    exact rename map, not a blanket exemption. (NRN-185 converged the verb/tool
//!    NAMES themselves — CLI `apply`/`repair` now share their MCP tool names
//!    `vault.apply`/`vault.repair`, so no tool-name rename carve-out remains; only
//!    per-surface field-shape renames stay.)
//! 5. **LOCAL-ONLY** — commands with no MCP tool at all: `completions`, `manpage`,
//!    `self-update`, `mcp`, `serve`, `cache`, `config edit`.
//! 6. **TRACKED GAP** — commands that *should* have an MCP twin but do not yet
//!    (`init`, `config show`/`validate`/`migrate`): NRN-188 / NRN-189. Distinct
//!    from LOCAL-ONLY: these are debts, not design.
//!
//! # Seeded field-level gaps (ship green; each tagged with its burndown task)
//!
//! There are currently no seeded field-presence gaps — the surface is at parity.
//!
//! Closed (kept here as burndown history): `vault.get`'s sort/paging + section
//! surface (`sort` / `desc` / `limit` / `no_limit` / `starts_at` / `section` /
//! `all_cols`, all NRN-173), `vault.set`'s coercing `--field` / `--push` /
//! `--pop` (NRN-181), `vault.move`'s `--force` / `--no-link-rewrite` (NRN-180),
//! and `vault.validate`'s `--summary` (NRN-182) all now have MCP twins, so their
//! allowlist entries were removed. (`vault.apply` gained `parents` in
//! lockstep with a new `apply --parents` under NRN-174, an identity pair that
//! never needed an allowlist entry.)
//!
//! # Out of scope for this *field-presence* gate (tracked separately)
//!
//! * **Exit-signal asymmetry** — NRN-183.
//! * **`vault.audit` `status` type refinement** — NRN-184, CLOSED. `status` was
//!   already *present* on both surfaces (so it never had a presence-gap allowlist
//!   entry), but MCP typed it as a free `String` where the CLI uses a closed
//!   enum. It now deserializes into a schema-carrying enum
//!   (`tools::audit::AuditStatusFilter`) that lowers through
//!   `cli::AuditStatus::as_str`, so the published `input_schema` advertises the
//!   `applied`/`skipped`/`failed` set and rejects typos. (A local mirror, not the
//!   CLI enum directly: `cli.rs` is `#[path]`-included by `build.rs`, whose
//!   build-script crate depends on neither `serde` nor `schemars`.) A presence
//!   test cannot express this refinement; it is verified by the `vault.audit`
//!   schema/behavior tests in `tests/mcp_audit.rs`.
//!
//! # Ratchet
//!
//! The allowlist is **exact-match in both directions**. Adding an unmapped CLI
//! flag fails (forward). Closing a seeded gap (e.g. NRN-180 adds `force` to
//! `vault.move`) makes its allowlist entry STALE and *also* fails — so burndown is
//! enforced, not just growth.
//!
//! # Deliberate limits of this model
//!
//! * **The gap class is one-directional (CLI-ahead).** Every seeded gap says "CLI
//!   has it, MCP lacks it"; there is no MCP-ahead gap class today because no MCP
//!   param leads the CLI. When the first MCP-first param arrives, add a
//!   `TrackedGapMcp` class (its mirror image) rather than abusing the naming map to
//!   launder an MCP-only field into apparent parity.
//! * **This is a *presence* gate, not a *semantics* gate.** Two surfaces can share
//!   a field name yet diverge in meaning (e.g. `get --col` narrows the projection
//!   on the CLI but only opts-in extra facets on MCP). A presence test cannot
//!   express that; such divergences are tracked per-field out of band (the `col`
//!   semantics break is deferred to the NRN-185/190 window), not papered over by
//!   the name match.

use std::collections::{BTreeMap, BTreeSet};

use clap::CommandFactory;

use super::server::McpServer;
use crate::cli::Cli;

/// How a CLI leaf command relates to the MCP surface.
enum Parity {
    /// Backed by an MCP tool; every CLI flag must map to an MCP field or a
    /// carve-out below.
    Mcp {
        tool: &'static str,
        /// CLI ids mapping to MCP `confirm` (class 1).
        safety: &'static [&'static str],
        /// CLI-only rendering knobs (class 2).
        presentation: &'static [&'static str],
        /// CLI-only ergonomic/surface-shape fields (class 3).
        shape: &'static [&'static str],
        /// `(cli_id, mcp_field)` sanctioned renames (class 4).
        naming: &'static [(&'static str, &'static str)],
        /// `(cli_id, task_id)` seeded field gaps: CLI has it, MCP lacks the
        /// same-named field.
        gaps: &'static [(&'static str, &'static str)],
    },
    /// No MCP tool by design (class 5).
    LocalOnly { why: &'static str },
    /// No MCP tool *yet* — a tracked debt (class 6).
    TrackedGap {
        task: &'static str,
        why: &'static str,
    },
}

struct Spec {
    /// clap leaf-command path, space-joined (e.g. `"config show"`).
    cli: &'static str,
    parity: Parity,
}

/// The declarative parity map. This doubles as parity documentation.
fn specs() -> Vec<Spec> {
    use Parity::*;
    vec![
        // ── Read surface ────────────────────────────────────────────────────
        Spec {
            cli: "find",
            parity: Mcp {
                tool: "vault.find",
                safety: &[],
                // `all` guards the missing-predicate help page; the MCP handler
                // hardcodes `all: true` (src/mcp/tools/find.rs) — a CLI-only guard.
                presentation: &["format", "no_pager"],
                shape: &["all"],
                naming: &[],
                gaps: &[],
            },
        },
        Spec {
            cli: "count",
            parity: Mcp {
                tool: "vault.count",
                safety: &[],
                presentation: &["format"],
                shape: &[],
                naming: &[],
                gaps: &[],
            },
        },
        Spec {
            cli: "describe",
            parity: Mcp {
                tool: "vault.describe",
                safety: &[],
                presentation: &["format"],
                // `--stats` is a pure alias for `--data`.
                shape: &["stats"],
                naming: &[],
                gaps: &[],
            },
        },
        Spec {
            cli: "get",
            parity: Mcp {
                tool: "vault.get",
                safety: &[],
                // `format` is now a real capability twin: `markdown` selects the
                // exact-source representation on both surfaces. Other CLI values
                // remain client renderers while MCP calls the default
                // `structured`; this presence gate cannot express enum-value
                // overlap, just like the documented `col` semantics divergence.
                presentation: &[],
                shape: &[],
                naming: &[],
                // NRN-173 CLOSED: vault.get now serves `sort`/`desc`/`limit`/
                // `no_limit`/`starts_at`/`section`/`all_cols` as identity-mapped
                // fields, so their seeded gap entries were removed. The `col`
                // SEMANTICS divergence (CLI narrows the projection; MCP only opts
                // facets in) remains — but `col` is *present* on both surfaces, so
                // it is not a presence gap and carries no entry here. That
                // semantics break is deferred to the NRN-185/190 window (see the
                // `col` doc on `GetParams` and this module's "presence gate, not a
                // semantics gate" note).
                gaps: &[],
            },
        },
        Spec {
            cli: "validate",
            parity: Mcp {
                tool: "vault.validate",
                safety: &[],
                presentation: &["format"],
                shape: &[],
                naming: &[],
                // NRN-182 CLOSED: vault.validate now serves the `summary` rollup
                // projection as an identity-mapped field.
                gaps: &[],
            },
        },
        Spec {
            cli: "repair",
            parity: Mcp {
                // NRN-185: converged — the CLI verb and the MCP tool share the
                // `repair` name (no rename carve-out). The tool drops its former
                // `_plan` suffix and leans on the invariant that nothing writes
                // except `apply`, so a bare `vault.repair` is unambiguously the
                // read-only plan-producing surface.
                tool: "vault.repair",
                safety: &[],
                // `--out` writes the plan artifact to a file vs stdout.
                presentation: &["format", "out"],
                // `--plan` selects plan-generation mode; vault.repair *is*
                // that mode (the bare findings-summary mode has no MCP surface).
                shape: &["plan"],
                naming: &[],
                gaps: &[],
            },
        },
        Spec {
            cli: "audit",
            parity: Mcp {
                tool: "vault.audit",
                safety: &[],
                presentation: &["format"],
                shape: &[],
                naming: &[],
                // NRN-184 (status stringly-typed) is a *type*-level gap: `status`
                // is present on both surfaces, so it is not a presence gap and has
                // no allowlist entry here. See module docs.
                gaps: &[],
            },
        },
        // ── Mutation surface ────────────────────────────────────────────────
        Spec {
            cli: "new",
            parity: Mcp {
                tool: "vault.new",
                safety: &["yes", "dry_run"],
                presentation: &["format"],
                shape: &[],
                naming: &[
                    ("as_rule", "rule"),
                    ("var", "vars"),
                    ("body_from_stdin", "body"),
                ],
                gaps: &[],
            },
        },
        Spec {
            cli: "set",
            parity: Mcp {
                tool: "vault.set",
                safety: &["yes", "dry_run"],
                presentation: &["format"],
                // NRN-208 (ADR 0010): trailing `key=value` positionals are pure
                // CLI-side input sugar that desugars to `--field` (the `fields`
                // id, which maps to MCP `field` below) before any plan is built.
                // No MCP twin needed — the canonical surface already has parity.
                shape: &["field_pos"],
                naming: &[
                    // MCP `body` is the wholesale-replacement analogue of stdin.
                    // The CLI's coercing `--field` (clap id `fields`) is the MCP
                    // `field` param (NRN-181) — a plural→singular rename mirroring
                    // vault.new's `field`.
                    ("body_from_stdin", "body"),
                    ("fields", "field"),
                ],
                // NRN-181/NRN-238 CLOSED: `field_json` / `push` / `pop` are now
                // identity-mapped MCP fields (ordered token lists, same name and
                // shape as `vault.new`'s `field_json`); the coercing `--field`
                // maps via the naming entry above.
                gaps: &[],
            },
        },
        Spec {
            cli: "edit",
            parity: Mcp {
                tool: "vault.edit",
                safety: &["yes", "dry_run"],
                presentation: &["format"],
                // NRN-210 (ADR 0010): single-op sugar + `--ops-file` are pure
                // CLI-side input surfaces that desugar 1:1 into the canonical ops
                // array (`edits_json` → MCP `edits`) before any plan is built. An
                // MCP client already sends the structured array directly, so
                // these need no MCP twin.
                shape: &[
                    "str_replace",
                    "replace_section",
                    "append_to_section",
                    "delete_section",
                    "insert_before_heading",
                    "insert_after_heading",
                    "new",
                    "content",
                    "replace_all",
                    "ops_file",
                ],
                // CLI takes the ops as a JSON string / stdin; MCP takes a
                // structured array under `edits`.
                naming: &[("edits_json", "edits")],
                gaps: &[],
            },
        },
        Spec {
            cli: "move",
            parity: Mcp {
                tool: "vault.move",
                safety: &["yes", "dry_run"],
                presentation: &["format"],
                shape: &[],
                naming: &[("src", "from"), ("dst", "to")],
                // NRN-180 CLOSED: vault.move now serves `force` + `no_link_rewrite`
                // as identity-mapped fields.
                gaps: &[],
            },
        },
        Spec {
            cli: "delete",
            parity: Mcp {
                tool: "vault.delete",
                safety: &["yes", "dry_run"],
                presentation: &["format"],
                shape: &[],
                naming: &[("doc", "target")],
                gaps: &[],
            },
        },
        Spec {
            cli: "rewrite-wikilink",
            parity: Mcp {
                tool: "vault.rewrite_wikilink",
                safety: &["yes", "dry_run"],
                presentation: &["format", "out"],
                shape: &[],
                naming: &[("old", "from"), ("new", "to")],
                gaps: &[],
            },
        },
        Spec {
            cli: "apply",
            parity: Mcp {
                // NRN-185: converged — CLI `apply` ↔ MCP `vault.apply` share the
                // `apply` name (no rename carve-out). Both execute a MigrationPlan;
                // `apply` is the plan-then-apply doctrine's execute verb.
                tool: "vault.apply",
                safety: &["yes", "dry_run"],
                // `--out` = report destination (stdout vs file).
                presentation: &["format", "out"],
                // `--input-format` selects the plan file's parse format; MCP takes
                // the plan as inline JSON, so there is no file format to detect.
                shape: &["input_format"],
                // CLI reads a plan file / stdin; MCP receives it inline as `plan`.
                naming: &[("plan_path", "plan")],
                gaps: &[],
            },
        },
        // ── Local-only (class 5) ────────────────────────────────────────────
        Spec {
            cli: "completions init",
            parity: LocalOnly {
                why: "shell completion script emission is a terminal-only concern",
            },
        },
        Spec {
            cli: "completions install",
            parity: LocalOnly {
                why: "writes into the user's shell config on this host",
            },
        },
        Spec {
            cli: "cache index",
            parity: LocalOnly {
                why: "explicit local cache management; MCP refreshes transparently",
            },
        },
        Spec {
            cli: "cache rebuild",
            parity: LocalOnly {
                why: "explicit local cache management; MCP refreshes transparently",
            },
        },
        Spec {
            cli: "cache clear",
            parity: LocalOnly {
                why: "explicit local cache management; MCP refreshes transparently",
            },
        },
        Spec {
            cli: "cache status",
            parity: LocalOnly {
                why: "explicit local cache management; MCP refreshes transparently",
            },
        },
        Spec {
            cli: "cache prune",
            parity: LocalOnly {
                why: "cross-vault local cache/state eviction on this host",
            },
        },
        Spec {
            cli: "cache sweep",
            parity: LocalOnly {
                why: "internal detached cross-vault cache/state GC child (NRN-287); hidden",
            },
        },
        Spec {
            cli: "config edit",
            parity: LocalOnly {
                why: "opens the config in $EDITOR — a local interactive action",
            },
        },
        Spec {
            cli: "manpage",
            parity: LocalOnly {
                why: "emits a roff man page for local install",
            },
        },
        Spec {
            cli: "self-update",
            parity: LocalOnly {
                why: "replaces the local binary from a GitHub release",
            },
        },
        Spec {
            cli: "mcp",
            parity: LocalOnly {
                why: "the MCP server launcher itself has no in-vault twin",
            },
        },
        Spec {
            cli: "serve",
            parity: LocalOnly {
                why: "the warm-host daemon launcher has no in-vault twin",
            },
        },
        // The launchd supervisor over `norn serve`: host-side process lifecycle,
        // not a vault capability — nothing for an in-vault MCP tool to twin.
        Spec {
            cli: "service install",
            parity: LocalOnly {
                why: "launchd supervision of the serve daemon has no in-vault twin",
            },
        },
        Spec {
            cli: "service uninstall",
            parity: LocalOnly {
                why: "launchd supervision of the serve daemon has no in-vault twin",
            },
        },
        Spec {
            cli: "service start",
            parity: LocalOnly {
                why: "launchd supervision of the serve daemon has no in-vault twin",
            },
        },
        Spec {
            cli: "service stop",
            parity: LocalOnly {
                why: "launchd supervision of the serve daemon has no in-vault twin",
            },
        },
        Spec {
            cli: "service restart",
            parity: LocalOnly {
                why: "launchd supervision of the serve daemon has no in-vault twin",
            },
        },
        Spec {
            cli: "service status",
            parity: LocalOnly {
                why: "launchd supervision of the serve daemon has no in-vault twin",
            },
        },
        // ── Tracked gaps (class 6) ──────────────────────────────────────────
        Spec {
            cli: "init",
            parity: TrackedGap {
                task: "NRN-188",
                why: "scaffolding .norn/config.yaml should be reachable over MCP",
            },
        },
        Spec {
            cli: "config show",
            parity: TrackedGap {
                task: "NRN-189",
                why: "config introspection should be reachable over MCP",
            },
        },
        Spec {
            cli: "config validate",
            parity: TrackedGap {
                task: "NRN-189",
                why: "config validation should be reachable over MCP",
            },
        },
        Spec {
            cli: "config migrate",
            parity: TrackedGap {
                task: "NRN-189",
                why: "config schema migration should be reachable over MCP",
            },
        },
    ]
}

/// The parameter names an MCP tool advertises in its published `input_schema`
/// (`tools/list`). Reads the live schema, so it cannot drift from the server.
fn tool_field_names(tool: &rmcp::model::Tool) -> BTreeSet<String> {
    tool.input_schema
        .get("properties")
        .and_then(|v| v.as_object())
        .map(|props| props.keys().cloned().collect())
        .unwrap_or_default()
}

/// Map of `tool name -> advertised param names` over the full served surface.
///
/// Consumes `McpServer::routers()` — the same seam `McpServer::new` builds
/// from — so a future third router lands here automatically without editing this
/// function. This enumerates every tool, read and mutation, which is what the
/// parity gate must check.
///
/// Panics on a duplicate tool name across routers: two tools sharing a name would
/// silently overwrite each other in the map, dropping one from the gate's view
/// (and it would be a real `tools/list` collision besides).
fn mcp_tools() -> BTreeMap<String, BTreeSet<String>> {
    let mut tools: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for router in McpServer::routers() {
        for tool in router.list_all() {
            let name = tool.name.to_string();
            let fields = tool_field_names(&tool);
            if tools.insert(name.clone(), fields).is_some() {
                panic!(
                    "duplicate MCP tool name '{name}' across routers — a name \
                     collision silently drops a tool from the parity gate and \
                     from tools/list; give each tool a unique name"
                );
            }
        }
    }
    tools
}

/// Walk the clap command tree to `leaf path -> {arg ids}`, excluding the ids in
/// `ignore` (global flags + the auto `help` flag).
fn cli_leaf_commands(ignore: &BTreeSet<String>) -> BTreeMap<String, BTreeSet<String>> {
    fn walk(
        cmd: &clap::Command,
        path: &str,
        ignore: &BTreeSet<String>,
        out: &mut BTreeMap<String, BTreeSet<String>>,
    ) {
        let mut has_sub = false;
        for sub in cmd.get_subcommands() {
            has_sub = true;
            let name = sub.get_name();
            let child = if path.is_empty() {
                name.to_string()
            } else {
                format!("{path} {name}")
            };
            walk(sub, &child, ignore, out);
        }
        if !has_sub && !path.is_empty() {
            let fields = cmd
                .get_arguments()
                .map(|a| a.get_id().as_str().to_string())
                .filter(|id| !ignore.contains(id))
                .collect();
            out.insert(path.to_string(), fields);
        }
    }

    let root = Cli::command();
    let mut out = BTreeMap::new();
    walk(&root, "", ignore, &mut out);
    out
}

/// Walk every NON-leaf (subcommand-bearing) node and record `path -> {non-global
/// arg ids}` for any that carries arguments of its own.
///
/// [`cli_leaf_commands`] models args only at leaves, so an arg declared on a
/// subcommand-bearing parent would never be parity-checked — and a leaf that
/// later grows a subcommand would silently drop all of its checks. This is the
/// tripwire: any hit here fails the gate loudly (see `cli_mcp_surface_parity`),
/// forcing the author to move the flag to a leaf or extend the gate's model.
fn cli_parent_args(ignore: &BTreeSet<String>) -> BTreeMap<String, BTreeSet<String>> {
    fn walk(
        cmd: &clap::Command,
        path: &str,
        ignore: &BTreeSet<String>,
        out: &mut BTreeMap<String, BTreeSet<String>>,
    ) {
        let mut has_sub = false;
        for sub in cmd.get_subcommands() {
            has_sub = true;
            let name = sub.get_name();
            let child = if path.is_empty() {
                name.to_string()
            } else {
                format!("{path} {name}")
            };
            walk(sub, &child, ignore, out);
        }
        // Skip the root (`path.is_empty()`): its args ARE the globals, already in
        // `ignore`. Record only non-root parents carrying their own non-global args.
        if has_sub && !path.is_empty() {
            let args: BTreeSet<String> = cmd
                .get_arguments()
                .map(|a| a.get_id().as_str().to_string())
                .filter(|id| !ignore.contains(id))
                .collect();
            if !args.is_empty() {
                out.insert(path.to_string(), args);
            }
        }
    }

    let root = Cli::command();
    let mut out = BTreeMap::new();
    walk(&root, "", ignore, &mut out);
    out
}

/// Global (root-level) arg ids, which every subcommand inherits — connection and
/// presentation concerns, not per-command capability.
fn global_ignore() -> BTreeSet<String> {
    let root = Cli::command();
    let mut ids: BTreeSet<String> = root
        .get_arguments()
        .map(|a| a.get_id().as_str().to_string())
        .collect();
    // Subcommands without `disable_help_flag` gain an auto `help` arg.
    ids.insert("help".to_string());
    ids
}

const POINTER: &str =
    "see the parity allowlist in src/mcp/parity_gate.rs and ADR 0009 (CLI and MCP are peer surfaces)";

/// Every served MCP tool must advertise an `outputSchema` in `tools/list`
/// (NRN-219). rmcp's `#[tool]` macro auto-derives `outputSchema` ONLY for the
/// literal `Json<T>` return type (it name-matches the `Json` identifier, it does
/// not inspect the `JsonSchema` trait), so a tool returning a custom wrapper —
/// e.g. `MutationResult<T>`, which exists to set `isError` on a not-applied
/// mutation — silently drops its schema unless it passes an explicit
/// `output_schema = …` attribute. `cli_mcp_surface_parity` reads only
/// `input_schema`, so that regression would ship green. This guard fails the
/// build, naming the offending tool.
#[test]
fn every_tool_advertises_an_output_schema() {
    let mut missing: Vec<String> = Vec::new();
    for router in McpServer::routers() {
        for tool in router.list_all() {
            if tool.output_schema.is_none() {
                missing.push(tool.name.to_string());
            }
        }
    }
    missing.sort();
    assert!(
        missing.is_empty(),
        "these MCP tools advertise no outputSchema in tools/list — a custom-wrapper \
         return type (e.g. MutationResult<T>) without an explicit `output_schema = …` \
         attribute drops it; see NRN-219: {missing:?}"
    );
}

/// Every tool that returns a non-`Json` wrapper (`MutationResult<T>`) publishes
/// its `outputSchema` via an explicit `output_schema = output_schema_for::<T>()`
/// attribute, because rmcp cannot auto-derive it for a non-`Json` return type
/// (NRN-219). That hand-written `T` is DECOUPLED from the return type, so a wrong
/// or stale `T` (a copy-paste, or a `T` left behind after an output-struct rename)
/// would advertise a schema describing the wrong payload while
/// `every_tool_advertises_an_output_schema` — a presence-only check — stays green.
///
/// This pins each tool's advertised schema to the schema of the `Output` it
/// actually returns, restoring the schema↔payload coupling that auto-derivation
/// gives the `Json<T>` read tools for free. A wrong/stale `T` whose schema
/// differs from the real payload fails here (a same-shaped swap is harmless — the
/// advertised schema still matches the payload — and correctly passes).
///
/// Covers all eight non-`Json` tools: the four cascade mutators (NRN-219), the
/// three single-op mutators converted in NRN-220, and the `vault.get` read that
/// carries `isError` (NRN-214). Add any future `MutationResult<T>` tool here.
#[test]
fn non_json_tools_advertise_their_payload_schema() {
    use crate::mcp::mutation_result::output_schema_for;
    use crate::mcp::tools::apply::ApplyOutput;
    use crate::mcp::tools::delete::DeleteOutput;
    use crate::mcp::tools::edit::EditOutput;
    use crate::mcp::tools::get::GetOutput;
    use crate::mcp::tools::move_doc::MoveOutput;
    use crate::mcp::tools::new::NewOutput;
    use crate::mcp::tools::rewrite_wikilink::RewriteWikilinkOutput;
    use crate::mcp::tools::set::SetOutput;

    // The contract: tool name → the schema of the payload type it returns.
    let expected = [
        ("vault.apply", output_schema_for::<ApplyOutput>()),
        ("vault.move", output_schema_for::<MoveOutput>()),
        ("vault.delete", output_schema_for::<DeleteOutput>()),
        (
            "vault.rewrite_wikilink",
            output_schema_for::<RewriteWikilinkOutput>(),
        ),
        ("vault.set", output_schema_for::<SetOutput>()),
        ("vault.edit", output_schema_for::<EditOutput>()),
        ("vault.new", output_schema_for::<NewOutput>()),
        ("vault.get", output_schema_for::<GetOutput>()),
    ];

    let mut advertised = BTreeMap::new();
    for router in McpServer::routers() {
        for tool in router.list_all() {
            advertised.insert(tool.name.to_string(), tool.output_schema.clone());
        }
    }

    for (name, want) in expected {
        let got = advertised
            .get(name)
            .cloned()
            .flatten()
            .unwrap_or_else(|| panic!("{name}: no outputSchema advertised"));
        assert_eq!(
            *got, *want,
            "{name} advertises an outputSchema that does not match the Output type it \
             returns — a wrong or stale `T` in its `output_schema = output_schema_for::<T>()` \
             attribute (see NRN-219)"
        );
    }
}

#[test]
fn cli_mcp_surface_parity() {
    let ignore = global_ignore();
    let cli = cli_leaf_commands(&ignore);
    let mcp = mcp_tools();
    let specs = specs();

    let mut violations: Vec<String> = Vec::new();

    // ── Tripwire: no subcommand-bearing node may carry its own non-global arg.
    // The gate models args only at leaves, so a flag on a parent (or on a leaf
    // that later grows a subcommand) would escape every parity check. Fail loudly
    // rather than pass a silent false green.
    for (parent, args) in cli_parent_args(&ignore) {
        let flags = args.iter().cloned().collect::<Vec<_>>().join(", ");
        violations.push(format!(
            "CLI command '{parent}' has subcommands AND carries its own non-global \
             arg(s) [{flags}]; the parity gate only models leaf args, so these escape \
             all parity checks. Move the flag(s) down to a leaf subcommand, or extend \
             the gate to model parent-node args ({POINTER})"
        ));
    }

    // Every CLI leaf must be covered by exactly one spec.
    let spec_paths: BTreeSet<&str> = specs.iter().map(|s| s.cli).collect();
    if spec_paths.len() != specs.len() {
        violations.push("duplicate CLI command in the parity allowlist".to_string());
    }
    for leaf in cli.keys() {
        if !spec_paths.contains(leaf.as_str()) {
            violations.push(format!(
                "CLI command '{leaf}' is not covered by the parity allowlist; \
                 add it as an MCP-backed, local-only, or tracked-gap entry ({POINTER})"
            ));
        }
    }

    // Every MCP tool must be claimed by exactly one Mcp spec.
    let mut claimed_tools: BTreeSet<&str> = BTreeSet::new();

    for spec in &specs {
        let cli_fields = match cli.get(spec.cli) {
            Some(f) => f,
            None => {
                violations.push(format!(
                    "parity allowlist references unknown CLI command '{}' ({POINTER})",
                    spec.cli
                ));
                continue;
            }
        };

        match &spec.parity {
            Parity::LocalOnly { why } => {
                // Whole command carved out (class 5); no field-level checks. A
                // stated justification is mandatory — an empty one is drift.
                if why.trim().is_empty() {
                    violations.push(format!(
                        "local-only command '{}' has no stated justification ({POINTER})",
                        spec.cli
                    ));
                }
                // A local-only command must NOT collide with a served MCP tool.
                // Normalize spaces/hyphens to `_` so multi-word and hyphenated
                // paths ("config edit", "self-update") map to their tool-name
                // shape ("vault.config_edit") — otherwise the guard could never
                // fire for them and would be dead for every such entry.
                let expected_tool = format!("vault.{}", spec.cli.replace([' ', '-'], "_"));
                if let Some(t) = mcp.keys().find(|t| t.as_str() == expected_tool) {
                    violations.push(format!(
                        "command '{}' is allowlisted local-only but MCP tool '{t}' exists — reclassify ({POINTER})",
                        spec.cli
                    ));
                }
            }
            Parity::TrackedGap { task, why } => {
                // Whole command carved out (class 6) but as a tracked debt: the
                // task id must be present and NRN-shaped, and the reason stated.
                if !task.starts_with("NRN-") {
                    violations.push(format!(
                        "tracked-gap command '{}' has a malformed task id '{task}' (expected NRN-XXX) ({POINTER})",
                        spec.cli
                    ));
                }
                if why.trim().is_empty() {
                    violations.push(format!(
                        "tracked-gap command '{}' has no stated justification ({POINTER})",
                        spec.cli
                    ));
                }
            }
            Parity::Mcp {
                tool,
                safety,
                presentation,
                shape,
                naming,
                gaps,
            } => {
                if !claimed_tools.insert(tool) {
                    violations.push(format!(
                        "MCP tool '{tool}' is claimed by more than one CLI command"
                    ));
                }
                let mcp_fields = match mcp.get(*tool) {
                    Some(f) => f,
                    None => {
                        violations.push(format!(
                            "CLI command '{}' maps to MCP tool '{tool}', which the server does not serve ({POINTER})",
                            spec.cli
                        ));
                        continue;
                    }
                };

                let safety_set: BTreeSet<&str> = safety.iter().copied().collect();
                let presentation_set: BTreeSet<&str> = presentation.iter().copied().collect();
                let shape_set: BTreeSet<&str> = shape.iter().copied().collect();
                let naming_cli: BTreeMap<&str, &str> = naming.iter().copied().collect();
                let naming_mcp: BTreeSet<&str> = naming.iter().map(|(_, m)| *m).collect();
                let gap_cli: BTreeMap<&str, &str> = gaps.iter().copied().collect();

                // ── Forward: every CLI flag must resolve ────────────────────
                for field in cli_fields {
                    let f = field.as_str();
                    if safety_set.contains(f)
                        || presentation_set.contains(f)
                        || shape_set.contains(f)
                    {
                        continue;
                    }
                    if let Some(mcp_field) = naming_cli.get(f) {
                        if !mcp_fields.contains(*mcp_field) {
                            violations.push(format!(
                                "'{}' --{f}: naming map points at MCP field '{mcp_field}' on tool '{tool}', which does not exist ({POINTER})",
                                spec.cli
                            ));
                        }
                        continue;
                    }
                    if let Some(task) = gap_cli.get(f) {
                        // A gap means the same-named MCP field is absent. If it now
                        // exists the gap is closed — the entry is STALE.
                        if mcp_fields.contains(f) {
                            violations.push(format!(
                                "'{}' --{f}: allowlisted as gap {task}, but MCP tool '{tool}' now has field '{f}' — gap closed, remove the stale allowlist entry ({POINTER})",
                                spec.cli
                            ));
                        }
                        continue;
                    }
                    if mcp_fields.contains(f) {
                        continue; // identity mapping
                    }
                    violations.push(format!(
                        "CLI '{}' flag --{f} has no MCP twin on tool '{tool}' and no carve-out; \
                         add an MCP param, a naming-map entry, a presentation/shape carve-out, or a tracked gap ({POINTER})",
                        spec.cli
                    ));
                }

                // ── Safety carve-out implies the tool has `confirm` ──────────
                if !safety_set.is_empty() && !mcp_fields.contains("confirm") {
                    violations.push(format!(
                        "'{}' uses the SAFETY carve-out (yes/dry_run -> confirm) but MCP tool '{tool}' has no 'confirm' field ({POINTER})",
                        spec.cli
                    ));
                }

                // ── Reverse: every MCP field must map back to the CLI ────────
                for mf in mcp_fields {
                    let m = mf.as_str();
                    if m == "confirm" {
                        if safety_set.is_empty() {
                            violations.push(format!(
                                "MCP tool '{tool}' has 'confirm' but CLI '{}' declares no SAFETY (yes/dry_run) mapping ({POINTER})",
                                spec.cli
                            ));
                        }
                        continue;
                    }
                    if naming_mcp.contains(m) {
                        continue;
                    }
                    // A name collision with a CLI carve-out id is NOT identity
                    // parity: presentation/shape/safety flags compute nothing the
                    // MCP surface can twin, so an MCP field that merely shares one
                    // of their names (`format`/`out`/`section`/`all`/…) would
                    // greenlight against a flag with no real capability behind it.
                    // Demand an explicit mapping instead of accepting the collision.
                    if presentation_set.contains(m)
                        || shape_set.contains(m)
                        || safety_set.contains(m)
                    {
                        violations.push(format!(
                            "MCP tool '{tool}' field '{m}' name-collides with CLI '{}' carve-out \
                             flag --{m} (presentation/shape/safety), which computes nothing on the \
                             MCP surface — an identity match here is a false green. Add an explicit \
                             naming-map entry, or back it with a real CLI capability twin ({POINTER})",
                            spec.cli
                        ));
                        continue;
                    }
                    if cli_fields.contains(m) {
                        continue; // identity
                    }
                    violations.push(format!(
                        "MCP tool '{tool}' field '{m}' has no CLI twin on '{}' and no carve-out ({POINTER})",
                        spec.cli
                    ));
                }

                // ── Staleness: no allowlist entry may reference a dead flag ──
                for s in safety.iter().chain(presentation.iter()).chain(shape.iter()) {
                    if !cli_fields.contains(*s) {
                        violations.push(format!(
                            "'{}' carve-out references flag --{s}, which the CLI no longer defines — stale allowlist entry ({POINTER})",
                            spec.cli
                        ));
                    }
                }
                for (c, mcp_field) in *naming {
                    if !cli_fields.contains(*c) {
                        violations.push(format!(
                            "'{}' naming map references CLI flag --{c}, which no longer exists — stale entry ({POINTER})",
                            spec.cli
                        ));
                    }
                    if !mcp_fields.contains(*mcp_field) {
                        violations.push(format!(
                            "'{}' naming map references MCP field '{mcp_field}' on '{tool}', which no longer exists — stale entry ({POINTER})",
                            spec.cli
                        ));
                    }
                }
                for (c, task) in *gaps {
                    // Governance: a seeded gap must name a real burndown task, the
                    // same discipline TrackedGap enforces — no `UNTRACKED` / empty
                    // placeholders that never get closed.
                    if task.trim().is_empty() || !task.starts_with("NRN-") {
                        violations.push(format!(
                            "'{}' gap for --{c} has a malformed task id '{task}' (expected NRN-XXX) ({POINTER})",
                            spec.cli
                        ));
                    }
                    if !cli_fields.contains(*c) {
                        violations.push(format!(
                            "'{}' gap {task} references CLI flag --{c}, which no longer exists — stale entry ({POINTER})",
                            spec.cli
                        ));
                    }
                }
            }
        }
    }

    // Every served MCP tool must be claimed by some CLI command.
    for tool in mcp.keys() {
        if !claimed_tools.contains(tool.as_str()) {
            violations.push(format!(
                "MCP tool '{tool}' is served but no CLI command maps to it; \
                 add it to the parity allowlist ({POINTER})"
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "CLI↔MCP parity broken ({} violation(s)):\n  - {}",
        violations.len(),
        violations.join("\n  - "),
    );
}
