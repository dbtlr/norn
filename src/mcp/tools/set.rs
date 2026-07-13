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

use anyhow::Result;
use serde::{Deserialize, Serialize};

use camino::Utf8PathBuf;

use crate::cli::{SetArgs, SetFormat};
use crate::mcp::context::VaultContext;
use crate::mcp::mutation_result::MutationResult;
use crate::set::report::{build_report, SetReport};

/// Parameters for `vault.set`.
///
/// `field_json` carries the frontmatter mutation as ordered `KEY=JSON` tokens
/// (the same shape `vault.new`'s `field_json` uses): each token sets that field
/// to the given JSON value, applied in order, and fed through the CLI's
/// `--field-json` seam, so they are coerced and schema-validated exactly as
/// `norn set --field-json` does — a key repeated across tokens accumulates into
/// an array. `remove` drops keys entirely. `body`, when present, wholly
/// replaces the document body (the MCP analogue of `norn set --body-from-stdin`).
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct SetParams {
    /// Target document (stem or path), as `norn set` accepts.
    pub target: String,

    /// Frontmatter fields to set, as raw `KEY=JSON` tokens, repeatable. Applied
    /// in order, and fed verbatim into `norn set --field-json KEY=JSON`: each
    /// value is JSON-parsed (scalar, array, object, or explicit null) and
    /// schema-validated exactly as the CLI flag is. A key repeated across
    /// tokens accumulates into an array (matching repeated `--field-json`
    /// flags). Empty list = no frontmatter change.
    #[serde(default)]
    pub field_json: Vec<String>,

    /// Frontmatter field overrides in `KEY=VALUE` format, repeatable. The value
    /// is string-coerced against the schema (dates, numbers, enums) exactly like
    /// `norn set --field KEY=VALUE` — the coercing counterpart to the typed
    /// `field_json` tokens. Use `field_json` when you need to pass a structured
    /// JSON value verbatim.
    #[serde(default)]
    pub field: Vec<String>,

    /// Append to a list-typed frontmatter field, as raw `KEY=VALUE` tokens,
    /// repeatable. Applied in order, and fed verbatim into `norn set --push
    /// KEY=VALUE`: each token appends one element, string-coerced against the
    /// schema exactly like the CLI flag. A key repeated across tokens pushes
    /// each value in turn (matching repeated `--push` flags). Creates a
    /// single-element array if the key does not exist.
    #[serde(default)]
    pub push: Vec<String>,

    /// Remove from a list-typed frontmatter field, as raw `KEY=VALUE` tokens,
    /// repeatable. Applied in order, and fed verbatim into `norn set --pop
    /// KEY=VALUE`: each token removes one member, string-coerced against the
    /// schema exactly like the CLI flag. A key repeated across tokens pops each
    /// value in turn (matching repeated `--pop` flags). Silent no-op for a
    /// member that is not present.
    #[serde(default)]
    pub pop: Vec<String>,

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
            // Prefer the error's resolved vault-relative path so `report.target`
            // means the same thing on refusal as on success (build_report uses the
            // resolved path). Fall back to the raw target for a coded refusal that
            // carries no path (an edit anchor miss identifies an anchor, not a doc).
            Some(err) => {
                let report_target = err
                    .path
                    .clone()
                    .map(Utf8PathBuf::from)
                    .unwrap_or_else(|| Utf8PathBuf::from(target));
                SetReport::refused(report_target, err)
            }
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

    // CONFIRM locks BEFORE any read that feeds the write; dry-run never locks.
    // See `crate::mcp::mutate::acquire_mutation_lock` for the invariant.
    let _mutation_lock = if p.confirm {
        Some(crate::mcp::mutate::acquire_mutation_lock(&cwd)?)
    } else {
        None
    };

    // ONE query_cache call serves both needs: the graph index (honoring
    // files.ignore, applied at cache-build time) and the cache handle for target
    // resolution come from the same snapshot — the pipeline (ground-shift,
    // freshness refresh) runs once per request. Warm-connection reuse under the
    // daemon; fresh open in cold mode (NRN-130).
    let config = ctx.config();
    let cache = ctx.query_cache()?;
    let index = cache.load_graph_index()?;

    let vault_cfg = &config.vault_config;

    // Build SetArgs inline, passing the ordered `field_json` / `push` / `pop`
    // token vectors straight through — the same pattern `vault.new`'s handler
    // uses for `field`/`field_json` (new.rs). Each is already the raw
    // `KEY=JSON` / `KEY=VALUE` token shape `preflight_and_plan` (via
    // `set::synth::synth_with_schema`) parses and schema-validates, so
    // malformed-token / type / allowed-values refusals surface as the same
    // coded errors the CLI produces — no map→token façade left to maintain.
    // `body` is handled below via the same synth_body_op seam the CLI's
    // --body-from-stdin path uses, so we leave body_from_stdin false (an MCP
    // server has no stdin).
    let args = SetArgs {
        target: p.target.clone(),
        // `field` is the coercing --field path (string coercion); `field_json`
        // is the typed --field-json path, both passed through verbatim.
        fields: p.field.clone(),
        // MCP passes fields via `field`; no CLI positional surface exists here.
        field_pos: Vec::new(),
        field_json: p.field_json.clone(),
        push: p.push.clone(),
        pop: p.pop.clone(),
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

    // CONFIRM: the mutation lock was already acquired above, before the
    // preflight read — apply now.

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
    let apply_report = apply_outcome?;

    // Warm mode: the apply committed on disk, so commit its cache increments as a
    // chunked writer-queue op (awaited) — the next read then finds the cache
    // current instead of paying a detect scan + rebuild (NRN-252 / NRN-158). A
    // no-op in cold mode.
    crate::mcp::mutate::commit_apply_increments(ctx, &apply_report.touched_paths());

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

        let report = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                field_json: vec![r#"status="active""#.into()],
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

        let report = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                field_json: vec![r#"status="active""#.into()],
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

    /// NRN-181/NRN-238: the coercing `field` param (KEY=VALUE) routes through the
    /// CLI's `--field` seam (string coercion against the schema), which is a
    /// *distinct* path from the JSON-typed `field_json` tokens (`--field-json`).
    ///
    /// The blind-spot fix (F4): a schemaless string value made both paths emit an
    /// identical `Value::String`, so a mis-wire (routing `field` through the typed
    /// seam) would go undetected. Here `up` is `wikilink`-typed, so the coercing
    /// path *wraps* a bare stem (`norn` -> `[[norn]]`) while the same bare string
    /// through the typed `field_json` path is refused (a bare string is not a
    /// shape-valid wikilink). The two paths therefore provably differ.
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

        // The SAME bare string through the typed `field_json` (--field-json) path
        // is refused — a bare `"norn"` is not a shape-valid wikilink — proving the
        // coercing and typed paths are distinct, not two names for one seam.
        let typed = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                field_json: vec![r#"up="norn""#.into()],
                confirm: false,
                ..Default::default()
            },
        );
        assert!(
            typed.is_err(),
            "the same bare string through the typed `field_json` path must be refused (not a valid \
             wikilink), proving the coercing --field path is a distinct seam"
        );
    }

    /// NRN-181/NRN-238: a `push` token appends a value to a list-typed field.
    #[test]
    fn confirm_push_appends_to_list() {
        let (_tmp, root) = seeded_vault_with_tags();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                push: vec!["tags=gamma".into()],
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

    /// NRN-181/NRN-238: a `pop` token removes a value from a list-typed field.
    #[test]
    fn confirm_pop_removes_from_list() {
        let (_tmp, root) = seeded_vault_with_tags();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                pop: vec!["tags=alpha".into()],
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

    /// NRN-238: repeating the SAME key across multiple `push` tokens accumulates
    /// N real members (order preserved) — the raw-token wire equivalent of the
    /// old map era's "array push explodes" test (an array-valued `push` map entry
    /// has no expression once `push` is an ordered token list; the CLI has always
    /// pushed multiple values via repeated `--push KEY=VALUE` flags, matching
    /// `norn set --push tags=gamma --push tags=delta`).
    #[test]
    fn confirm_push_duplicate_key_accumulates_all_members() {
        let (_tmp, root) = seeded_vault_with_tags();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                push: vec!["tags=gamma".into(), "tags=delta".into()],
                confirm: true,
                ..Default::default()
            },
        )
        .expect("handle (duplicate-key push) should succeed");

        assert!(report.applied, "duplicate-key push must apply");
        let content = std::fs::read_to_string(root.join("task.md")).unwrap();
        // Both new members land as real list elements alongside the originals.
        for member in ["alpha", "beta", "gamma", "delta"] {
            assert!(
                content.contains(member),
                "duplicate-key push must append each member as a real element ({member} missing):\n{content}"
            );
        }
        // Never stringify anything into a literal list/array element.
        assert!(
            !content.contains("[[") && !content.contains("[\""),
            "duplicate-key push must NOT append a literal array element:\n{content}"
        );
    }

    /// NRN-238: repeating the SAME key across multiple `pop` tokens removes each
    /// named member; the untouched members survive — the raw-token wire
    /// equivalent of the old map era's "array pop removes named members" test
    /// (matching `norn set --pop tags=alpha --pop tags=gamma`).
    #[test]
    fn confirm_pop_duplicate_key_removes_named_members() {
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

        let report = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                pop: vec!["tags=alpha".into(), "tags=gamma".into()],
                confirm: true,
                ..Default::default()
            },
        )
        .expect("handle (duplicate-key pop) should succeed");

        assert!(report.applied, "duplicate-key pop must apply");
        let content = std::fs::read_to_string(root.join("task.md")).unwrap();
        assert!(
            !content.contains("alpha") && !content.contains("gamma"),
            "duplicate-key pop must remove every named member:\n{content}"
        );
        assert!(
            content.contains("beta"),
            "duplicate-key pop must leave the untouched members:\n{content}"
        );
    }

    /// NRN-238: a `push` VALUE that happens to look like JSON (`{"nested":
    /// "object"}`) is treated as an opaque string, exactly like `norn set --push
    /// tags='{"nested":"object"}'` — `--push` has no JSON parsing, so the raw
    /// token shape can no longer refuse a "structured" push value the way the
    /// map-era `expand_list_ops` façade did (there is no wire form left that
    /// carries a JSON object into `push`/`pop`: every token is one scalar
    /// `KEY=VALUE` pair). Verified against the live CLI (`cargo run -- set
    /// --push tags='{"nested":"object"}'`) before writing this assertion, per the
    /// migration mandate to match CLI token behavior exactly rather than guess.
    #[test]
    fn push_json_shaped_value_is_literal_string_not_parsed() {
        let (_tmp, root) = seeded_vault_with_tags();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                push: vec![r#"tags={"nested":"object"}"#.into()],
                confirm: true,
                ..Default::default()
            },
        )
        .expect("handle (JSON-shaped push value) should succeed — --push never parses JSON");

        assert!(report.applied, "JSON-shaped push value must apply");
        let content = std::fs::read_to_string(root.join("task.md")).unwrap();
        assert!(
            content.contains(r#"{"nested":"object"}"#),
            "the JSON-shaped value must land as a literal string list element:\n{content}"
        );
    }

    /// F5: `push` on an ABSENT key creates a single-element array (add_frontmatter).
    #[test]
    fn confirm_push_absent_key_creates_single_element_array() {
        // seeded_vault has no `tags` field at all.
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                push: vec!["tags=solo".into()],
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

        let report = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                // `zeta` is not a member of [alpha, beta].
                pop: vec!["tags=zeta".into()],
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
