//! `vault.set` — schema-aware single-document frontmatter (and body) mutation.
//!
//! This is the FIRST MCP mutation tool, and it establishes the **mutation-safety
//! contract** every later mutation tool (`vault.new`, `vault.move`,
//! `vault.delete`, `vault.apply`) copies:
//!
//! - **Default DRY-RUN.** A call WITHOUT `confirm: true` runs the full
//!   preflight/plan and returns the report with `applied = false`, acquiring NO
//!   mutation lock and writing NOTHING to disk.
//! - **`confirm: true` WRITES.** It acquires the per-vault mutation lock and
//!   applies the plan, returning the report with `applied = true`.
//!
//! Every MCP call is effectively non-TTY, so this mirrors `norn set`'s non-TTY
//! semantics exactly: the CLI's "non-TTY without --yes = implicit dry-run" path
//! maps onto `confirm = false`, and the CLI's `--yes` apply path maps onto
//! `confirm = true`. Same preflight (`set::synth::preflight_and_plan`), same
//! mutation lock (`mutation_lock::MutationLock`), same applier
//! (`repair_apply::apply_repair_plan_with_context`), same trace-id source (a
//! telemetry `EventSink` minted exactly as the CLI mints it) — so `vault.set`
//! and `norn set` cannot drift on resolution, schema enforcement, or apply
//! semantics.
//!
//! The one deliberate difference: the CLI reads a wholesale body replacement
//! from stdin (`--body-from-stdin`); an MCP client has no stdin, so the body
//! travels as a `body` param and the body op is synthesized via the same
//! `set::synth::synth_body_op` seam the CLI's stdin path uses.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use camino::Utf8PathBuf;

use crate::cli::{SetArgs, SetFormat};
use crate::mcp::context::VaultContext;
use crate::mcp::mutation_result::MutationResult;
use crate::set::report::{build_report, SetReport};

/// Parameters for `vault.set`.
///
/// `set` is the frontmatter mutation map: each `field -> value` pair sets that
/// field to the given JSON value. Values travel as JSON (scalars, arrays,
/// objects, explicit null) and are fed through the CLI's `--field-json` seam, so
/// they are coerced and schema-validated exactly as `norn set --field-json`
/// does. `remove` drops keys entirely. `body`, when present, wholly replaces the
/// document body (the MCP analogue of `norn set --body-from-stdin`).
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct SetParams {
    /// Target document (stem or path), as `norn set` accepts.
    pub target: String,

    /// Frontmatter fields to set: `field -> JSON value`. Each value is applied
    /// verbatim (scalar, array, object, or null) and schema-validated like
    /// `norn set --field-json field=<json>`. Empty map = no frontmatter change.
    #[serde(default)]
    pub set: BTreeMap<String, serde_json::Value>,

    /// Frontmatter field overrides in `KEY=VALUE` format, repeatable. The value
    /// is string-coerced against the schema (dates, numbers, enums) exactly like
    /// `norn set --field KEY=VALUE` — the coercing counterpart to the typed `set`
    /// map. Use `set` when you need to pass a structured JSON value verbatim.
    #[serde(default)]
    pub field: Vec<String>,

    /// Append to a list-typed frontmatter field: `field -> value`. A scalar
    /// value appends one element; an **array** value appends each element in
    /// order (equivalent to repeating `norn set --push KEY=VALUE` once per
    /// element). Creates a single-element array if the key does not exist.
    /// Values are string-coerced like `norn set --push KEY=VALUE`. An object
    /// value, or an array containing a nested array/object, is refused — never
    /// stringified.
    #[serde(default)]
    pub push: BTreeMap<String, serde_json::Value>,

    /// Remove from a list-typed frontmatter field: `field -> value`. A scalar
    /// value removes one member; an **array** value removes each named member in
    /// order. Silent no-op for a member that is not present. String-coerced like
    /// `norn set --pop KEY=VALUE`. An object value, or an array containing a
    /// nested array/object, is refused — never stringified.
    #[serde(default)]
    pub pop: BTreeMap<String, serde_json::Value>,

    /// Frontmatter keys to remove entirely. Silent no-op for missing keys, like
    /// `norn set --remove key`.
    #[serde(default)]
    pub remove: Vec<String>,

    /// Wholesale body replacement. When present, the document body (everything
    /// after the frontmatter) is replaced with this string — the MCP analogue of
    /// `norn set --body-from-stdin`. Absent = body unchanged.
    #[serde(default)]
    pub body: Option<String>,

    /// Bypass schema enforcement (type validation + required-field protection),
    /// mirroring `norn set --force`.
    #[serde(default)]
    pub force: bool,

    /// Apply the mutation. **Defaults to `false` (dry-run): the call returns the
    /// planned change with `applied = false` and writes nothing.** Pass `true` to
    /// acquire the vault mutation lock and write the change to disk.
    #[serde(default)]
    pub confirm: bool,
}

/// Structured output for `vault.set`.
///
/// rmcp requires a tool's advertised `outputSchema` to have a root `type:
/// object`. [`SetReport`] carries a `camino::Utf8PathBuf` target field, which has
/// no `schemars::JsonSchema` impl, so the report cannot derive `JsonSchema`
/// directly. We wrap it as a generic `serde_json::Value` inside this typed
/// envelope (the same pattern `vault.get` / `vault.validate` use): the full
/// report structure travels faithfully in the JSON; only the inner schema is
/// left generic.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SetOutput {
    /// The `SetReport` JSON: the planned (or applied) frontmatter changes, the
    /// `applied` flag, body-change sizing, warnings, and (on apply) the trace id.
    /// Byte-for-byte the same shape `norn set --format json` emits.
    pub report: serde_json::Value,
}

impl SetOutput {
    fn from_report(report: &SetReport) -> Result<Self> {
        Ok(Self {
            report: serde_json::to_value(report)?,
        })
    }
}

/// Render a JSON **scalar** as the bare `VALUE` half of a `KEY=VALUE` argument
/// for the coercing `--push` / `--pop` seam. A JSON string yields its unquoted
/// contents (`"done"` -> `done`); a number/bool/null yields its compact JSON
/// form (`5`, `true`, `null`), which `infer_scalar` then coerces exactly as the
/// CLI does. An array or object is **not** a scalar and yields `None` — the
/// caller refuses it rather than stringifying a structured value into a single
/// literal list element.
fn scalar_arg(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(_) | serde_json::Value::Bool(_) | serde_json::Value::Null => {
            Some(v.to_string())
        }
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => None,
    }
}

/// Expand a `push` / `pop` map into the flat `KEY=VALUE` vector the CLI
/// `--push` / `--pop` seam consumes.
///
/// A scalar value produces one `KEY=VALUE` entry. An **array** value explodes
/// into N sequential entries (order preserved), byte-for-byte equivalent to
/// repeating the CLI flag once per element — so `push {tags: [a, b]}` appends
/// two real members, not one literal `["a","b"]` element.
///
/// An **object** value, or an array element that is itself an array/object, is
/// refused with a params error: these have no scalar `KEY=VALUE` form, and
/// stringifying one would silently append a literal `{...}`/`[...]` element (a
/// silent-corruption bug). `flag` names the offending param (`push`/`pop`) in
/// the error.
fn expand_list_ops(map: &BTreeMap<String, serde_json::Value>, flag: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for (key, value) in map {
        match value {
            serde_json::Value::Array(items) => {
                for item in items {
                    let scalar = scalar_arg(item).ok_or_else(|| {
                        anyhow::anyhow!(
                            "{flag} value for '{key}' must be a scalar or a flat array of \
                             scalars; a nested array/object list element is not allowed"
                        )
                    })?;
                    out.push(format!("{key}={scalar}"));
                }
            }
            scalar_or_object => {
                let scalar = scalar_arg(scalar_or_object).ok_or_else(|| {
                    anyhow::anyhow!(
                        "{flag} value for '{key}' must be a scalar or a flat array of \
                         scalars, not an object"
                    )
                })?;
                out.push(format!("{key}={scalar}"));
            }
        }
    }
    Ok(out)
}

/// Build the MCP output envelope for `vault.set`: run the pure handler, then
/// project the report into the typed [`SetOutput`]. The single function the
/// `#[tool]` wrapper calls.
pub fn handle_output(ctx: &VaultContext, p: SetParams) -> Result<MutationResult<SetOutput>> {
    let dry_run = !p.confirm;
    let target = p.target.clone();
    // Capture a coded refusal (NRN-220): a recognized precondition/CAS refusal
    // becomes a structured `refused` report + `isError:true`, not a bare MCP
    // `Err` with the code laundered to prose. Unrecognized errors still propagate.
    let report = match handle(ctx, p) {
        Ok(report) => report,
        Err(e) => match crate::mcp::mutate::refusal_from_error(&e) {
            Some(err) => SetReport::refused(Utf8PathBuf::from(target), err),
            None => return Err(e),
        },
    };
    let outcome = report.outcome;
    Ok(MutationResult::from_outcome(
        SetOutput::from_report(&report)?,
        dry_run,
        outcome,
    ))
}

/// Pure handler for `vault.set`.
///
/// Mirrors `norn set`'s dispatch (see `main.rs` `Command::Set`):
/// load config → load the graph index (honoring `files.ignore`) → open a query
/// cache → `preflight_and_plan` → DRY-RUN unless `confirm`. On `confirm`, acquire
/// the per-vault mutation lock and apply via the shared repair applier.
///
/// **Safety invariant:** when `!confirm`, this acquires NO lock and never calls
/// the applier — it returns `build_report(.., applied = false, ..)` and leaves
/// the file untouched.
pub fn handle(ctx: &VaultContext, p: SetParams) -> Result<SetReport> {
    let cwd = ctx.vault_root.clone();

    // Load the graph index honoring files.ignore, exactly like the CLI set path.
    let config = ctx.config();
    let index = crate::cache_cmd::load_graph_index(&cwd, &config.index_options, false)?;

    // Cache for target resolution (needs document query, not just the index).
    let cache = ctx.query_cache()?;

    let vault_cfg = &config.vault_config;

    // Build SetArgs inline. The MCP `set` map routes through --field-json so JSON
    // values (scalars, arrays, objects, null) are applied verbatim and
    // schema-validated like the CLI. `body` is handled below via the same
    // synth_body_op seam the CLI's --body-from-stdin path uses, so we leave
    // body_from_stdin false (an MCP server has no stdin).
    let field_json: Vec<String> = p
        .set
        .iter()
        .map(|(k, v)| Ok(format!("{k}={}", serde_json::to_string(v)?)))
        .collect::<Result<Vec<_>>>()?;

    // `push` / `pop` maps route through the CLI's string-coercing --push/--pop
    // seam (infer_scalar), so each value renders as a bare KEY=VALUE string (not
    // JSON-quoted) — matching `norn set --push status=done`. An array value
    // explodes into N sequential entries (matching repeated CLI flags); an
    // object, or a nested array/object element, is refused rather than
    // stringified into a literal list element.
    let push = expand_list_ops(&p.push, "push")?;
    let pop = expand_list_ops(&p.pop, "pop")?;

    let args = SetArgs {
        target: p.target.clone(),
        // `field` is the coercing --field path (string coercion); `set` is the
        // typed --field-json path routed above.
        fields: p.field.clone(),
        // MCP passes fields via `field`; no CLI positional surface exists here.
        field_pos: Vec::new(),
        field_json,
        push,
        pop,
        remove: p.remove.clone(),
        body_from_stdin: false,
        force: p.force,
        // `yes` / `dry_run` are CLI-TTY knobs; the MCP contract is driven by
        // `confirm` below. preflight_and_plan does not read either field, so
        // their values here are inert.
        yes: false,
        dry_run: false,
        format: SetFormat::Json,
    };

    let mut outcome =
        crate::set::synth::preflight_and_plan(&cwd, &cache, &index, vault_cfg, &args)?;

    // Body replacement: the CLI reads this from stdin in step 8 of
    // preflight_and_plan; an MCP client has none, so we synthesize the same
    // `replace_body` op here via the identical `synth_body_op` seam and stamp it
    // exactly as preflight stamps every other change (path + doc hash +
    // change_id). This keeps `vault.set` body semantics byte-identical to
    // `norn set --body-from-stdin`.
    if let Some(new_body) = p.body.as_deref() {
        crate::set::synth::inject_body_change(&cwd, &mut outcome, new_body)?;
    }

    // DRY-RUN (default): no lock, no apply, no write.
    if !p.confirm {
        return Ok(build_report(&outcome, false, ""));
    }

    // CONFIRM: acquire the per-vault mutation lock, then apply.
    let _mutation_lock = crate::mcp::mutate::acquire_mutation_lock(&cwd)?;

    // Open a REAL, file-backed event sink on the apply path — the same audit
    // trail `norn set --yes` writes (lifecycle → op_planned → action → finished).
    // This is what makes an MCP-applied mutation "audited for free": an
    // off-filesystem client still leaves the append-only event stream a CLI
    // mutation would. Best-effort by contract (falls back to discard if the file
    // can't be opened), so telemetry never blocks the mutation. The sink also
    // owns the trace id stamped into the report.
    let mut sink = crate::mcp::mutate::open_mutation_event_sink(ctx);
    crate::emit_invocation_started(
        &mut sink,
        "set",
        &cwd,
        outcome.plan.vault_root.as_str(),
        /*dry_run=*/ false,
        &["set".to_string(), p.target.clone()],
    );

    // Pre-stamp an op span per planned change so the applier's per-op action
    // events thread under their `op_planned` span — same as the CLI path.
    let spans = crate::repair_apply::build_op_spans(&mut sink, &outcome.plan.changes);

    let apply_outcome = crate::repair_apply::apply_repair_plan_with_context(
        &cwd,
        &index,
        &outcome.plan,
        /*dry_run=*/ false,
        &crate::repair_apply::CreateApplyContext::default(),
        &mut sink,
        &spans,
        None,
    );
    let trace_id = sink.trace_id().to_string();
    let exit = if apply_outcome.is_ok() { 0 } else { 2 };
    crate::emit_single_op_finished(&mut sink, "set", exit, apply_outcome.is_ok());
    apply_outcome?;

    Ok(build_report(&outcome, true, &trace_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    /// Seed a temp vault with a single `task.md` carrying `status: backlog`.
    fn seeded_vault() -> (TempDir, Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-set-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(
            root.join("task.md"),
            "---\ntype: task\nstatus: backlog\n---\nTask body\n",
        )
        .unwrap();
        (tmp, root)
    }

    fn disk_status(root: &Utf8PathBuf) -> String {
        let content = std::fs::read_to_string(root.join("task.md")).unwrap();
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("status:") {
                return rest.trim().to_string();
            }
        }
        panic!("no status field in task.md:\n{content}");
    }

    /// The heart of the mutation-safety contract: a call WITHOUT confirm runs the
    /// plan, reports `applied = false`, and writes NOTHING to disk.
    #[test]
    fn dry_run_default_writes_nothing() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let mut set = BTreeMap::new();
        set.insert("status".to_string(), serde_json::json!("active"));

        let report = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                set,
                remove: Vec::new(),
                body: None,
                force: false,
                confirm: false,
                ..Default::default()
            },
        )
        .expect("handle (dry-run) should succeed");

        assert!(!report.applied, "dry-run report must have applied == false");
        assert_eq!(
            disk_status(&root),
            "backlog",
            "dry-run must leave the file on disk UNCHANGED (status still backlog)"
        );
    }

    /// `confirm: true` acquires the lock, applies, reports `applied = true`, and
    /// the file on disk reflects the change.
    #[test]
    fn confirm_writes_the_change() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let mut set = BTreeMap::new();
        set.insert("status".to_string(), serde_json::json!("active"));

        let report = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                set,
                remove: Vec::new(),
                body: None,
                force: false,
                confirm: true,
                ..Default::default()
            },
        )
        .expect("handle (confirm) should succeed");

        assert!(report.applied, "confirm report must have applied == true");
        assert_eq!(
            disk_status(&root),
            "active",
            "confirm must write the change to disk (status now active)"
        );
    }

    /// Body replacement (the `--body-from-stdin` analogue) under confirm rewrites
    /// the body and preserves frontmatter.
    #[test]
    fn confirm_replaces_body() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                set: BTreeMap::new(),
                remove: Vec::new(),
                body: Some("Replaced body\n".into()),
                force: false,
                confirm: true,
                ..Default::default()
            },
        )
        .expect("handle (confirm body) should succeed");

        assert!(report.applied);
        assert!(report.body_changed, "body_changed should be true");
        let content = std::fs::read_to_string(root.join("task.md")).unwrap();
        assert!(
            content.contains("Replaced body"),
            "body should be replaced on disk:\n{content}"
        );
        assert!(
            content.contains("status: backlog"),
            "frontmatter must be preserved:\n{content}"
        );
    }

    /// Dry-run body replacement reports the change but writes nothing.
    #[test]
    fn dry_run_body_writes_nothing() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                set: BTreeMap::new(),
                remove: Vec::new(),
                body: Some("Replaced body\n".into()),
                force: false,
                confirm: false,
                ..Default::default()
            },
        )
        .expect("handle (dry-run body) should succeed");

        assert!(!report.applied);
        assert!(
            report.body_changed,
            "dry-run still reports the planned body change"
        );
        let content = std::fs::read_to_string(root.join("task.md")).unwrap();
        assert!(
            content.contains("Task body"),
            "dry-run must leave the original body on disk:\n{content}"
        );
        assert!(
            !content.contains("Replaced body"),
            "dry-run must NOT write the new body:\n{content}"
        );
    }

    /// Seed a temp vault with a `task.md` carrying a list-typed `tags` field.
    fn seeded_vault_with_tags() -> (TempDir, Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-set-tags-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(
            root.join("task.md"),
            "---\ntype: task\nstatus: backlog\ntags:\n  - alpha\n  - beta\n---\nTask body\n",
        )
        .unwrap();
        (tmp, root)
    }

    /// Seed a temp vault with a `task.md` and a schema declaring a `wikilink`-typed
    /// `up` field, so the coercing `--field` path and the typed `--field-json`
    /// (`set`) path diverge on the same input.
    fn seeded_vault_with_wikilink_schema() -> (TempDir, Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-set-wikischema-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let config_dir = root.join(".norn");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.yaml"),
            "validate:\n  rules:\n    - name: task-fields\n      match:\n        \
             frontmatter:\n          type: task\n      field_types:\n        up: wikilink\n",
        )
        .unwrap();
        std::fs::write(
            root.join("task.md"),
            "---\ntype: task\nstatus: backlog\n---\nTask body\n",
        )
        .unwrap();
        (tmp, root)
    }

    fn disk_field<'a>(content: &'a str, field: &str) -> Option<&'a str> {
        let prefix = format!("{field}:");
        content
            .lines()
            .find_map(|l| l.strip_prefix(&prefix))
            .map(str::trim)
    }

    /// NRN-181: the coercing `field` param (KEY=VALUE) routes through the CLI's
    /// `--field` seam (string coercion against the schema), which is a *distinct*
    /// path from the JSON-typed `set` map (`--field-json`).
    ///
    /// The blind-spot fix (F4): a schemaless string value made both paths emit an
    /// identical `Value::String`, so a mis-wire (routing `field` through the typed
    /// seam) would go undetected. Here `up` is `wikilink`-typed, so the coercing
    /// path *wraps* a bare stem (`norn` -> `[[norn]]`) while the same bare string
    /// through the typed `set` path is refused (a bare string is not a shape-valid
    /// wikilink). The two paths therefore provably differ.
    #[test]
    fn confirm_field_coerces_and_writes() {
        let (_tmp, root) = seeded_vault_with_wikilink_schema();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        // Coercing --field path: bare stem is wrapped into a wikilink and written.
        let report = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                field: vec!["up=norn".to_string()],
                confirm: true,
                ..Default::default()
            },
        )
        .expect("handle (field) should succeed");

        assert!(report.applied, "field mutation must apply");
        let content = std::fs::read_to_string(root.join("task.md")).unwrap();
        let up = disk_field(&content, "up")
            .unwrap_or_else(|| panic!("up field must be written:\n{content}"));
        assert!(
            up.contains("[[norn]]"),
            "coercing field param must wrap the bare stem into a wikilink on disk, got: {up}\n{content}"
        );

        // The SAME bare string through the typed `set` (--field-json) path is
        // refused — a bare `"norn"` is not a shape-valid wikilink — proving the
        // coercing and typed paths are distinct, not two names for one seam.
        let mut set = BTreeMap::new();
        set.insert("up".to_string(), serde_json::json!("norn"));
        let typed = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                set,
                confirm: false,
                ..Default::default()
            },
        );
        assert!(
            typed.is_err(),
            "the same bare string through the typed `set` path must be refused (not a valid wikilink), \
             proving the coercing --field path is a distinct seam"
        );
    }

    /// NRN-181: the `push` map appends a value to a list-typed field.
    #[test]
    fn confirm_push_appends_to_list() {
        let (_tmp, root) = seeded_vault_with_tags();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let mut push = BTreeMap::new();
        push.insert("tags".to_string(), serde_json::json!("gamma"));

        let report = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                push,
                confirm: true,
                ..Default::default()
            },
        )
        .expect("handle (push) should succeed");

        assert!(report.applied, "push mutation must apply");
        let content = std::fs::read_to_string(root.join("task.md")).unwrap();
        assert!(
            content.contains("gamma"),
            "push must append gamma to the tags list:\n{content}"
        );
        // The existing members survive the append.
        assert!(
            content.contains("alpha") && content.contains("beta"),
            "push must preserve existing list members:\n{content}"
        );
    }

    /// NRN-181: the `pop` map removes a value from a list-typed field.
    #[test]
    fn confirm_pop_removes_from_list() {
        let (_tmp, root) = seeded_vault_with_tags();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let mut pop = BTreeMap::new();
        pop.insert("tags".to_string(), serde_json::json!("alpha"));

        let report = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                pop,
                confirm: true,
                ..Default::default()
            },
        )
        .expect("handle (pop) should succeed");

        assert!(report.applied, "pop mutation must apply");
        let content = std::fs::read_to_string(root.join("task.md")).unwrap();
        assert!(
            !content.contains("alpha"),
            "pop must remove alpha from the tags list:\n{content}"
        );
        assert!(
            content.contains("beta"),
            "pop must leave the other list members:\n{content}"
        );
    }

    /// F1: an **array** `push` value explodes into N real members (order
    /// preserved), matching repeated `norn set --push` flags — not one literal
    /// `["gamma","delta"]` element.
    #[test]
    fn confirm_push_array_appends_all_members() {
        let (_tmp, root) = seeded_vault_with_tags();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let mut push = BTreeMap::new();
        push.insert("tags".to_string(), serde_json::json!(["gamma", "delta"]));

        let report = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                push,
                confirm: true,
                ..Default::default()
            },
        )
        .expect("handle (array push) should succeed");

        assert!(report.applied, "array push must apply");
        let content = std::fs::read_to_string(root.join("task.md")).unwrap();
        // Both new members land as real list elements alongside the originals.
        for member in ["alpha", "beta", "gamma", "delta"] {
            assert!(
                content.contains(member),
                "array push must append each member as a real element ({member} missing):\n{content}"
            );
        }
        // Never stringify the array into one literal element.
        assert!(
            !content.contains("[[") && !content.contains("[\""),
            "array push must NOT append a literal array element:\n{content}"
        );
    }

    /// F1: an **array** `pop` value removes each named member; the untouched
    /// members survive.
    #[test]
    fn confirm_pop_array_removes_named_members() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-set-poparr-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(
            root.join("task.md"),
            "---\ntype: task\nstatus: backlog\ntags:\n  - alpha\n  - beta\n  - gamma\n---\nTask body\n",
        )
        .unwrap();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let mut pop = BTreeMap::new();
        pop.insert("tags".to_string(), serde_json::json!(["alpha", "gamma"]));

        let report = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                pop,
                confirm: true,
                ..Default::default()
            },
        )
        .expect("handle (array pop) should succeed");

        assert!(report.applied, "array pop must apply");
        let content = std::fs::read_to_string(root.join("task.md")).unwrap();
        assert!(
            !content.contains("alpha") && !content.contains("gamma"),
            "array pop must remove every named member:\n{content}"
        );
        assert!(
            content.contains("beta"),
            "array pop must leave the untouched members:\n{content}"
        );
    }

    /// F1: an **object** `push` value has no scalar KEY=VALUE form and is refused
    /// with a params error — never stringified into a literal `{...}` element.
    #[test]
    fn push_object_value_is_refused() {
        let (_tmp, root) = seeded_vault_with_tags();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let mut push = BTreeMap::new();
        push.insert("tags".to_string(), serde_json::json!({"nested": "object"}));

        let result = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                push,
                confirm: true,
                ..Default::default()
            },
        );
        assert!(
            result.is_err(),
            "an object push value must be refused, not stringified"
        );
        // Disk is untouched: the refusal happens before any lock or write.
        let content = std::fs::read_to_string(root.join("task.md")).unwrap();
        assert!(
            !content.contains("nested"),
            "a refused object push must write nothing:\n{content}"
        );
    }

    /// F1: an array `pop` value containing a nested array/object element is
    /// refused — the nested element has no scalar form.
    #[test]
    fn pop_nested_array_element_is_refused() {
        let (_tmp, root) = seeded_vault_with_tags();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let mut pop = BTreeMap::new();
        pop.insert("tags".to_string(), serde_json::json!(["alpha", ["nested"]]));

        let result = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                pop,
                confirm: true,
                ..Default::default()
            },
        );
        assert!(
            result.is_err(),
            "a nested-array pop element must be refused, not stringified"
        );
    }

    /// F5: `push` on an ABSENT key creates a single-element array (add_frontmatter).
    #[test]
    fn confirm_push_absent_key_creates_single_element_array() {
        // seeded_vault has no `tags` field at all.
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let mut push = BTreeMap::new();
        push.insert("tags".to_string(), serde_json::json!("solo"));

        let report = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                push,
                confirm: true,
                ..Default::default()
            },
        )
        .expect("handle (push absent key) should succeed");

        assert!(report.applied, "push on an absent key must apply");
        let content = std::fs::read_to_string(root.join("task.md")).unwrap();
        // The field is created carrying exactly the one pushed element.
        assert!(
            content.contains("tags:") && content.contains("solo"),
            "push on an absent key must create a single-element tags array:\n{content}"
        );
    }

    /// F5: `pop` of an ABSENT value is a silent success no-op — the call applies,
    /// emits no error, and leaves the list untouched.
    #[test]
    fn confirm_pop_absent_value_is_silent_noop() {
        let (_tmp, root) = seeded_vault_with_tags();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let mut pop = BTreeMap::new();
        // `zeta` is not a member of [alpha, beta].
        pop.insert("tags".to_string(), serde_json::json!("zeta"));

        let report = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                pop,
                confirm: true,
                ..Default::default()
            },
        )
        .expect("handle (pop absent value) should succeed");

        // Silent success: the call applies with no error and no body change,
        // matching `norn set --pop tags=zeta` on a value that is not present.
        assert!(
            report.applied,
            "pop of an absent value must still apply cleanly"
        );
        assert!(
            !report.body_changed,
            "pop of an absent value must not touch the body"
        );
        let content = std::fs::read_to_string(root.join("task.md")).unwrap();
        assert!(
            content.contains("alpha") && content.contains("beta"),
            "pop of an absent value must leave every existing member:\n{content}"
        );
        assert!(
            !content.contains("zeta"),
            "pop of an absent value must not introduce it:\n{content}"
        );
    }
}
