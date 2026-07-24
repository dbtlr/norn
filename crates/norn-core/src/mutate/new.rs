//! The `new` execute seam: three-mode document creation (explicit path / by
//! rule / inbox), schema-default resolution, and a single `create_document`
//! `MigrationPlan` applied through the shared executor.
//!
//! Every clean pre-write decline — a resolve/preflight/synth refusal — returns a
//! `Refused` report carrying a coded error, never a bare `Err`.

use std::collections::{BTreeMap, BTreeSet};

use super::coerce;
use super::{owner_index_options, MutationExecution};
use crate::apply::fsops::ensure_within_vault;
use crate::apply::{apply_migration_plan, ApplyContext};
use crate::domain::GraphIndex;
use crate::seq_alloc::{self, SEQ_TOKEN};
use crate::standards::{
    applicable_rules, compile_config, path_variables, render, resolve_to_fixpoint, CompiledConfig,
    Context, VaultConfig,
};
use camino::{Utf8Path, Utf8PathBuf};
use chrono::{NaiveDate, NaiveDateTime};
use norn_wire::{ApplyOutcome, OpStatus};
use norn_wire::{
    CodedError, FrontmatterCreated, MutationOutcome, MutationWarning, NewParams, NewReport,
};
use norn_wire::{MigrationOp, MigrationPlan, MIGRATION_PLAN_SCHEMA_VERSION};
use serde_json::{Map, Value};

/// Internal placeholder that survives the substitution engine untouched — NUL
/// bytes never occur in a template or a real path.
const SEQ_SENTINEL: &str = "\u{0}norn_seq\u{0}";

/// Execute a `new`: forecast (`confirm == false`) or apply (`confirm == true`).
pub fn execute(
    cache: &crate::cache::Cache,
    config: Option<&crate::standards::VaultConfig>,
    params: &norn_wire::NewParams,
    today: &str,
    sink: &mut crate::telemetry::EventSink,
) -> anyhow::Result<MutationExecution<norn_wire::NewReport>> {
    let default_config = VaultConfig::default();
    let cfg = config.unwrap_or(&default_config);

    let vault_root = cache.vault_root().to_owned();
    let source_path = vault_root.join(".norn/config.yaml");
    let compiled = match compile_config(cfg, &source_path) {
        Ok(c) => c,
        Err(e) => return Ok(refused_new(refusal("config-invalid", e.to_string(), None))),
    };
    let index = cache.load_graph_index()?;
    let now = parse_now(today)?;

    // ── --var KEY=VALUE ──────────────────────────────────────────────────────
    let mut vars: BTreeMap<String, String> = BTreeMap::new();
    for kv in &params.vars {
        match coerce::split_kv(kv) {
            Some((k, v)) => {
                vars.insert(k, v);
            }
            None => {
                return Ok(refused_new(refusal(
                    "assignment-malformed",
                    format!("invalid --var format (expected KEY=VALUE): {kv}"),
                    None,
                )))
            }
        }
    }

    // ── Three-mode target resolution ─────────────────────────────────────────
    let resolved = match resolve_target(cfg, params, &vars, now) {
        Ok(r) => r,
        Err(r) => return Ok(refused_new(r)),
    };
    let doc_path = resolved.path;

    // ── Preflight (containment / .md / dotfile / dest-exists / parent) ───────
    if let Err(r) = preflight(&vault_root, &doc_path, params.force, params.parents) {
        return Ok(refused_new(r));
    }

    // ── Body precedence: stdin > rule scaffold > empty ───────────────────────
    let body = match resolve_body(
        params,
        &resolved.body_scaffold,
        &resolved.path_vars,
        now,
        cfg,
    ) {
        Ok(b) => b,
        Err(r) => return Ok(refused_new(r)),
    };
    let body_bytes = body.len();

    // ── Schema-default resolution + provenance + warnings ────────────────────
    let mut built = match build_create(
        cfg,
        &compiled,
        &index,
        &doc_path,
        &resolved.path_vars,
        params,
        now,
    ) {
        Ok(b) => b,
        Err(r) => return Ok(refused_new(r)),
    };

    // NRN-37c: --title is inert in Mode A (explicit path).
    if params.path.is_some() && params.as_rule.is_none() {
        if let Some(title) = &params.title {
            built.warnings.push(MutationWarning {
                code: "title-ignored".into(),
                field: None,
                // The `title-ignored:` records prefix is added by the display layer
                // per-code; the message is the bare detail.
                message: format!("--title '{title}' has no effect with an explicit path"),
            });
        }
    }

    // ── Build the single create_document MigrationOp ─────────────────────────
    let fm_map: Map<String, Value> = built.frontmatter.into_iter().collect();
    let mut new_value = Map::new();
    new_value.insert("frontmatter".into(), Value::Object(fm_map));
    new_value.insert("body".into(), Value::String(body.clone()));
    let mut fields = Map::new();
    fields.insert("path".into(), Value::String(doc_path.to_string()));
    fields.insert("new_value".into(), Value::Object(new_value));
    fields.insert("force".into(), Value::Bool(params.force));

    let plan = MigrationPlan {
        schema_version: MIGRATION_PLAN_SCHEMA_VERSION,
        vault_root: vault_root.to_string(),
        generator: None,
        generated_at: None,
        preconditions: Vec::new(),
        operations: vec![MigrationOp {
            kind: "create_document".to_string(),
            id: None,
            requires: Vec::new(),
            fields: Value::Object(fields),
            footnote: None,
        }],
        skipped: Vec::new(),
        plan_footnote: None,
    };

    // ── Apply (forecast writes nothing) ──────────────────────────────────────
    let ctx = ApplyContext {
        dry_run: !params.confirm,
        parents: params.parents,
        verbose: false,
        refuse_as_report: true,
        owner_index_options: owner_index_options(config),
    };
    let apply_report = apply_migration_plan(&plan, &index, ctx, sink)?;

    if matches!(
        apply_report.outcome,
        ApplyOutcome::Refused | ApplyOutcome::Failed
    ) {
        let coded = apply_report
            .operations
            .iter()
            .find(|o| o.status == OpStatus::Failed)
            .and_then(|o| o.error.clone())
            .map(|e| CodedError {
                code: e.code,
                message: e.message,
                path: e.path,
            })
            .unwrap_or_else(|| CodedError {
                code: "internal-error".into(),
                message: "apply refused without a coded op error".into(),
                path: None,
            });
        return Ok(refused_new(Refusal {
            code_owned: Some(coded.code.clone()),
            code: "",
            message: coded.message.clone(),
            path: coded.path.clone(),
        }));
    }

    let applied = params.confirm;
    let (path, predicted_path) = if applied {
        let resolved_path = apply_report
            .operations
            .iter()
            .find(|o| o.kind == "create_document")
            .and_then(|o| o.path.clone())
            .unwrap_or_else(|| doc_path.to_string());
        (Some(resolved_path), None)
    } else {
        let predicted = if seq_alloc::has_seq(&doc_path) {
            seq_alloc::predict(&vault_root, &doc_path)
                .ok()
                .map(|p| p.to_string())
        } else {
            None
        };
        (Some(doc_path.to_string()), predicted)
    };
    let touched_paths = if applied {
        apply_report.touched_paths.clone()
    } else {
        Vec::new()
    };
    // Real trace id on a confirmed apply, empty on a forecast (NRN-400) — see
    // `mutate::set::execute`. The executor mints it from the EventSink on a
    // write, so it correlates to the durable telemetry line this apply wrote.
    let trace_id = apply_report.trace_id.clone();

    // Post-create validate pass. Only meaningful once the file is actually on
    // disk (a forecast writes nothing for the pass to see), so it runs only on a
    // confirmed apply, never on the dry-run preview path.
    let mut warnings = built.warnings;
    if applied {
        if let Some(created_path) = path.as_deref() {
            let extra = post_create_validate(
                cfg,
                &compiled,
                &vault_root,
                config,
                &index,
                created_path,
                &warnings,
            );
            warnings.extend(extra);
        }
    }

    Ok(MutationExecution {
        report: NewReport {
            schema_version: 2,
            trace_id,
            telemetry_degraded: apply_report.telemetry_degraded,
            operation: "new".into(),
            path,
            applied,
            outcome: if applied {
                MutationOutcome::Applied
            } else {
                MutationOutcome::Forecast
            },
            frontmatter_created: built.created,
            body_bytes,
            warnings,
            predicted_path,
            error: None,
        },
        touched_paths,
    })
}

// ── Post-create validate (NRN-390) ──────────────────────────────────────────

/// Run the general validate engine over the created document and surface any
/// finding for it that `build_create`'s hand-computed warnings
/// (missing-required/unknown-field/wikilink/stem-collision) don't already cover.
///
/// Scope is the created path, not the vault: the warm pre-create graph
/// (`baseline`) is overlaid with just the new file re-read from disk (the write
/// already landed under `apply_migration_plan`; the owner's cache increment for
/// `touched_paths` commits later). Link resolution runs across the composite so
/// the new document's own links resolve against the whole vault, but only the one
/// changed path is read from disk. Findings are then filtered to the created doc.
/// Alias checks are skipped (`alias_field: None`) for this pass.
///
/// Dedup: a `RequiredFrontmatterMissing` finding whose field is already covered
/// by a synth-phase `missing-required-field` warning is dropped and every other
/// finding surfaces once, recoded onto the `missing-required-field` family when
/// it IS a required-field miss the synth phase didn't already catch. Every other
/// finding kind reuses the engine's own `code`/`message` verbatim, plus the
/// finding's own field when its body carries one (the unified
/// `{code, field?, message}` envelope is richer than a flat
/// `ValidationFinding{code,message}`).
#[allow(clippy::too_many_arguments)]
fn post_create_validate(
    cfg: &VaultConfig,
    compiled: &CompiledConfig,
    vault_root: &Utf8Path,
    config: Option<&VaultConfig>,
    baseline: &GraphIndex,
    doc_path: &str,
    existing_warnings: &[MutationWarning],
) -> Vec<MutationWarning> {
    let index_options = super::owner_index_options(config);
    let mut fresh_index = baseline.clone();
    crate::graph::overlay_changed_paths(
        &mut fresh_index,
        vault_root,
        &[Utf8PathBuf::from(doc_path)],
        &index_options,
    );

    let findings =
        crate::standards::validate_with_compiled(&fresh_index, &cfg.validate, compiled);

    let already_warned: BTreeSet<&str> = existing_warnings
        .iter()
        .filter(|w| w.code == "missing-required-field")
        .filter_map(|w| w.field.as_deref())
        .collect();

    // allowed_values is now enforced at preflight (NRN-430): a non-force create
    // never reaches this pass with a violation (it refused), and a --force create
    // already carries a `force-bypass` warning for the field. Drop the engine's
    // `value-not-allowed` finding for a field already bypass-warned so the force
    // path reports the bypass once rather than doubling it.
    let force_bypassed: BTreeSet<&str> = existing_warnings
        .iter()
        .filter(|w| w.code == "force-bypass")
        .filter_map(|w| w.field.as_deref())
        .collect();

    let mut extra = Vec::new();
    for finding in findings.iter().filter(|f| f.path.as_str() == doc_path) {
        if finding.code == "value-not-allowed"
            && finding
                .field
                .as_deref()
                .is_some_and(|f| force_bypassed.contains(f))
        {
            continue;
        }
        if finding.code == "frontmatter-required-field-missing" {
            let Some(field) = finding.field.as_deref() else {
                continue;
            };
            if already_warned.contains(field) {
                continue;
            }
            let rules = finding
                .rule
                .as_ref()
                .map(|r| vec![r.clone()])
                .unwrap_or_default();
            extra.push(MutationWarning {
                code: "missing-required-field".into(),
                field: Some(field.to_string()),
                message: format!(
                    "missing required field '{field}' (rules: {})",
                    rules.join(", ")
                ),
            });
        } else {
            extra.push(MutationWarning {
                code: finding.code.clone(),
                field: finding.field.clone(),
                message: finding.message.clone(),
            });
        }
    }
    extra
}

// ── Three-mode resolution ───────────────────────────────────────────────────

struct ResolvedTarget {
    path: Utf8PathBuf,
    path_vars: BTreeMap<String, String>,
    body_scaffold: Option<String>,
}

fn resolve_target(
    cfg: &VaultConfig,
    params: &NewParams,
    vars: &BTreeMap<String, String>,
    now: NaiveDateTime,
) -> Result<ResolvedTarget, Refusal> {
    match (params.path.as_deref(), params.as_rule.as_deref()) {
        (Some(_), Some(_)) => Err(refusal(
            "path-and-rule-conflict",
            "pass either a path or --as, not both",
            None,
        )),
        (Some(p), None) => Ok(ResolvedTarget {
            path: Utf8PathBuf::from(p),
            path_vars: BTreeMap::new(),
            body_scaffold: None,
        }),
        (None, Some(name)) => {
            let rule = cfg
                .validate
                .rules
                .iter()
                .find(|r| r.name.as_deref() == Some(name))
                .ok_or_else(|| refusal("unknown-rule", format!("unknown rule `{name}`"), None))?;
            let target = rule.target.as_deref().ok_or_else(|| {
                refusal(
                    "rule-not-creatable",
                    format!("rule `{name}` is not creatable (no `target`)"),
                    None,
                )
            })?;
            let generated = generate_path(target, params.title.as_deref(), vars, now, cfg)?;
            Ok(ResolvedTarget {
                path: Utf8PathBuf::from(generated),
                path_vars: vars.clone(),
                body_scaffold: rule.body.clone(),
            })
        }
        (None, None) => {
            let inbox = cfg.inbox.path.as_deref().ok_or_else(|| {
                refusal(
                    "no-inbox-configured",
                    "no path, no --as, and no inbox configured",
                    None,
                )
            })?;
            let title = params.title.as_deref().ok_or_else(|| {
                refusal(
                    "inbox-requires-title",
                    "inbox creation requires --title",
                    None,
                )
            })?;
            let target = format!("{inbox}/{{{{title|slugify}}}}.md");
            let generated = generate_path(&target, Some(title), vars, now, cfg)?;
            Ok(ResolvedTarget {
                path: Utf8PathBuf::from(generated),
                path_vars: vars.clone(),
                body_scaffold: None,
            })
        }
    }
}

/// Render a `target` template into a concrete path. `{{seq}}` is shielded so it
/// survives to apply-time allocation; a misplaced `{{seq}}` is refused here.
fn generate_path(
    target: &str,
    title: Option<&str>,
    vars: &BTreeMap<String, String>,
    now: NaiveDateTime,
    cfg: &VaultConfig,
) -> Result<String, Refusal> {
    for name in referenced_vars(target) {
        if !vars.contains_key(&name) {
            return Err(refusal(
                "missing-var",
                format!(
                    "missing required template variable `{name}` (supply with --var {name}=...)"
                ),
                None,
            ));
        }
    }
    if references_title(target) && title.is_none() {
        return Err(refusal(
            "missing-title",
            "this target needs a title (supply with --title)",
            None,
        ));
    }

    let ctx = Context {
        now,
        title: title.unwrap_or("").to_string(),
        path_vars: vars.clone(),
        date_format: cfg.templates.date_format.clone(),
        time_format: cfg.templates.time_format.clone(),
    };
    let protected = target.replace(SEQ_TOKEN, SEQ_SENTINEL);
    let rendered = render(&protected, &ctx).map_err(|e| {
        refusal(
            "template-render-failed",
            format!("template error: {e}"),
            None,
        )
    })?;
    let out = rendered.replace(SEQ_SENTINEL, SEQ_TOKEN);
    if seq_alloc::seq_misplaced(Utf8Path::new(&out)) {
        return Err(refusal(
            "seq-misplaced",
            "`{{seq}}` is only supported once, in the file name of a rule target",
            None,
        ));
    }
    Ok(out)
}

/// Base names of every `{{ ... }}` token (before any `|` transform).
fn referenced_tokens(target: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = target;
    while let Some(s) = rest.find("{{") {
        let after = &rest[s + 2..];
        let Some(e) = after.find("}}") else { break };
        let inner = after[..e].split('|').next().unwrap_or("").trim();
        if !inner.is_empty() && !out.contains(&inner.to_string()) {
            out.push(inner.to_string());
        }
        rest = &after[e + 2..];
    }
    out
}

fn referenced_vars(target: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in referenced_tokens(target) {
        if let Some(n) = token
            .strip_prefix("var.")
            .or_else(|| token.strip_prefix("path."))
        {
            if !n.is_empty() && !out.contains(&n.to_string()) {
                out.push(n.to_string());
            }
        }
    }
    out
}

fn references_title(target: &str) -> bool {
    referenced_tokens(target).iter().any(|t| t == "title")
}

// ── Preflight ────────────────────────────────────────────────────────────────

fn preflight(
    vault_root: &Utf8Path,
    rel: &Utf8Path,
    force: bool,
    parents: bool,
) -> Result<(), Refusal> {
    let rel_str = rel.as_str();
    if !rel_str.ends_with(".md") {
        return Err(refusal(
            "not-markdown",
            format!("path must end in .md: {rel_str}"),
            Some(rel_str.to_string()),
        ));
    }
    let canonical_root = vault_root.as_std_path().canonicalize().map_err(|e| {
        refusal(
            "containment-unresolvable",
            format!(
                "refusing to operate on '{rel_str}': cannot verify vault-root containment ({e})"
            ),
            Some(rel_str.to_string()),
        )
    })?;
    if let Err(c) = ensure_within_vault(vault_root, &canonical_root, rel) {
        return Err(refusal_owned(
            c.code(),
            c.to_string(),
            Some(c.target().to_string()),
        ));
    }
    if rel_str.split('/').any(|seg| seg.starts_with('.')) {
        return Err(refusal(
            "dotfile-path",
            format!("dotfile paths are excluded from vaults: {rel_str}"),
            Some(rel_str.to_string()),
        ));
    }
    let full = vault_root.join(rel);
    if full.as_std_path().exists() && !force {
        return Err(refusal(
            "destination-exists",
            format!("destination already exists (use --force to overwrite): {rel_str}"),
            Some(rel_str.to_string()),
        ));
    }
    if let Some(parent) = full.parent() {
        if !parent.as_std_path().exists() && !parents {
            return Err(refusal(
                "parent-missing",
                format!("parent directory does not exist (use -p / --parents to auto-create): {rel_str}"),
                Some(rel_str.to_string()),
            ));
        }
    }
    Ok(())
}

fn resolve_body(
    params: &NewParams,
    scaffold: &Option<String>,
    path_vars: &BTreeMap<String, String>,
    now: NaiveDateTime,
    cfg: &VaultConfig,
) -> Result<String, Refusal> {
    if let Some(b) = &params.body {
        // Trim a single trailing newline (shell `echo` convention).
        Ok(b.strip_suffix('\n').unwrap_or(b).to_string())
    } else if let Some(s) = scaffold {
        let ctx = Context {
            now,
            title: params.title.clone().unwrap_or_default(),
            path_vars: path_vars.clone(),
            date_format: cfg.templates.date_format.clone(),
            time_format: cfg.templates.time_format.clone(),
        };
        render(s, &ctx).map_err(|e| {
            refusal(
                "template-render-failed",
                format!("body scaffold render error: {e}"),
                None,
            )
        })
    } else {
        Ok(String::new())
    }
}

// ── Schema-default resolution ───────────────────────────────────────────────

struct CreateBuilt {
    frontmatter: BTreeMap<String, Value>,
    created: Vec<FrontmatterCreated>,
    warnings: Vec<MutationWarning>,
}

#[derive(Clone, Copy)]
enum OverrideSrc {
    Field,
    FieldJson,
}

fn build_create(
    cfg: &VaultConfig,
    compiled: &CompiledConfig,
    index: &GraphIndex,
    doc_path: &Utf8Path,
    extra_path_vars: &BTreeMap<String, String>,
    params: &NewParams,
    now: NaiveDateTime,
) -> Result<CreateBuilt, Refusal> {
    // Hard boundary: refuse to create a files.ignore'd path (NRN-131).
    if crate::graph::is_ignored(doc_path, &cfg.files.ignore) {
        return Err(refusal(
            "path-ignored",
            format!(
                "cannot create {doc_path}: excluded by files.ignore (norn does not manage ignored paths)"
            ),
            Some(doc_path.to_string()),
        ));
    }

    // Path variables: pattern captures (first-rule-wins) + --var fills holes.
    let mut path_vars: BTreeMap<String, String> = BTreeMap::new();
    for crule in &compiled.rules {
        for (k, v) in path_variables(crule, doc_path.as_str()) {
            path_vars.entry(k).or_insert(v);
        }
    }
    for (k, v) in extra_path_vars {
        path_vars.entry(k.clone()).or_insert_with(|| v.clone());
    }

    // Operator overrides (--field / --field-json).
    let mut raw: Vec<(String, Value, OverrideSrc)> = Vec::new();
    for kv in &params.fields {
        let (k, v) = coerce::split_kv(kv).ok_or_else(|| {
            refusal(
                "assignment-malformed",
                format!("expected KEY=VALUE, got: {kv}"),
                None,
            )
        })?;
        raw.push((k, Value::String(v), OverrideSrc::Field));
    }
    for kv in &params.field_json {
        let (k, rawj) = coerce::split_kv(kv).ok_or_else(|| {
            refusal(
                "assignment-malformed",
                format!("expected KEY=VALUE, got: {kv}"),
                None,
            )
        })?;
        let parsed: Value = serde_json::from_str(&rawj).map_err(|e| {
            refusal(
                "field-json-invalid",
                format!("--field-json value is not valid JSON ({k}): {e}"),
                None,
            )
        })?;
        raw.push((k, parsed, OverrideSrc::FieldJson));
    }

    // Schema-aware coercion of --field overrides (unless --force).
    let path_only_rules = applicable_rules(cfg, compiled, doc_path.as_str(), None);
    let mut operator_overrides: BTreeMap<String, Value> = BTreeMap::new();
    // Track WHICH operator flag supplied each override so the report can credit
    // the three-way source vocabulary (operator-flag / operator-flag-json
    // / schema-default) rather than a flattened two-way label.
    let mut override_src: BTreeMap<String, OverrideSrc> = BTreeMap::new();
    for (key, value, src) in raw {
        override_src.insert(key.clone(), src);
        let coerced = if matches!(src, OverrideSrc::Field) && !params.force {
            let raw_str = value.as_str().unwrap_or("");
            let spec = path_only_rules
                .iter()
                .find_map(|(rule, _)| rule.field_types.get(&key));
            match spec.and_then(|s| s.type_name()) {
                Some(ty) => coerce::coerce_value_for_type(
                    ty,
                    raw_str,
                    spec.and_then(|s| s.effective_max_length()),
                )
                .map_err(|e| refusal_owned(e.code(), e.to_string(), Some(doc_path.to_string())))?,
                None => coerce::infer_scalar(raw_str),
            }
        } else {
            value
        };
        operator_overrides.insert(key, coerced);
    }

    // Fixpoint resolution.
    let (resolved_fm, applied_rule_names) = resolve_to_fixpoint(
        cfg,
        compiled,
        doc_path.as_str(),
        now,
        &operator_overrides,
        &path_vars,
    )
    .map_err(|e| refusal("substitution-failed", e.to_string(), None))?;

    // Provenance: credit each field to override / the first matching default rule.
    let resolved_fm_value = Value::Object(
        resolved_fm
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    );
    let matched_rules =
        applicable_rules(cfg, compiled, doc_path.as_str(), Some(&resolved_fm_value));

    let unknown_fields: BTreeSet<String> = operator_overrides
        .keys()
        .filter(|key| !coerce::field_known_in_rules(matched_rules.iter().map(|(r, _)| *r), key))
        .cloned()
        .collect();

    // allowed_values enforcement (NRN-430). Every field in the synthesized
    // frontmatter is a fresh direct write, so a value outside a matching rule's
    // allowed_values set REFUSES here at forecast/preflight — before any file
    // lands — with --force the documented bypass. `build_create`'s type coercion
    // never checked allowed_values (a validate-engine-only rule), so without this
    // a clean forecast could precede a schema-violating create, defeating
    // plan-then-apply. The allowed list rides in the refusal message so an agent
    // can recover without a second query; the code/message converge on `set`'s
    // `value-not-allowed` family. Every matching rule that declares
    // allowed_values for the field is checked — not just the first — so two
    // co-applying rules with different sets both gate the value; a scan
    // stopping at the first declaring rule would let a value valid under it
    // but rejected by a second rule pass preflight, surfacing only as the
    // post-apply warning.
    let mut allowed_bypass: Vec<MutationWarning> = Vec::new();
    for (field, value) in &resolved_fm {
        let mut bypassed = false;
        for (rule, _) in &matched_rules {
            let Some(allowed) = rule.allowed_values.get(field) else {
                continue;
            };
            if coerce::value_in_allowed(value, allowed) {
                continue;
            }
            if !params.force {
                return Err(refusal(
                    "value-not-allowed",
                    coerce::value_not_allowed_message(
                        field,
                        &coerce::display_value(value),
                        &coerce::display_allowed(allowed),
                    ),
                    Some(doc_path.to_string()),
                ));
            }
            if !bypassed {
                allowed_bypass.push(coerce::force_bypass_warning(
                    field,
                    "allowed-values validation",
                ));
                bypassed = true;
            }
        }
    }

    let mut created: Vec<FrontmatterCreated> = Vec::new();
    for (field, value) in &resolved_fm {
        if operator_overrides.contains_key(field) {
            // Source vocabulary: a `--field` override is `operator-flag`, a
            // `--field-json` override is `operator-flag-json` — the distinction
            // the flattened label lost.
            let source = match override_src.get(field) {
                Some(OverrideSrc::FieldJson) => "operator-flag-json",
                _ => "operator-flag",
            };
            created.push(FrontmatterCreated {
                field: field.clone(),
                value: value.clone(),
                source: source.into(),
                rule: None,
            });
        } else {
            let rule = matched_rules
                .iter()
                .find(|(r, _)| r.frontmatter_defaults.contains_key(field))
                .and_then(|(r, _)| r.name.clone());
            created.push(FrontmatterCreated {
                field: field.clone(),
                value: value.clone(),
                source: "schema-default".into(),
                rule,
            });
        }
    }

    // Warnings (non-blocking). The --force allowed-values bypasses computed above
    // lead so a bypassed field is legible before the derived warnings.
    let mut warnings: Vec<MutationWarning> = allowed_bypass;
    for field in unknown_fields {
        warnings.push(MutationWarning {
            code: "unknown-field".into(),
            field: Some(field.clone()),
            message: format!("field '{field}' not declared in schema"),
        });
    }
    for (field, value) in &resolved_fm {
        if let Some(s) = value.as_str() {
            if s.starts_with("[[") && s.ends_with("]]") {
                warnings.extend(super::wikilink_warnings(index, field, s));
            }
        }
    }
    let mut missing: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for rule in &cfg.validate.rules {
        let rule_name = rule.name.clone().unwrap_or_default();
        if !applied_rule_names.contains(&rule_name) {
            continue;
        }
        for required in &rule.required_frontmatter {
            if !resolved_fm.contains_key(required) {
                missing
                    .entry(required.clone())
                    .or_default()
                    .push(rule_name.clone());
            }
        }
    }
    for (field, rules) in missing {
        warnings.push(MutationWarning {
            code: "missing-required-field".into(),
            field: Some(field.clone()),
            message: format!(
                "missing required field '{field}' (rules: {})",
                rules.join(", ")
            ),
        });
    }
    let new_stem = doc_path.file_stem().unwrap_or("").to_lowercase();
    let collisions: Vec<String> = index
        .documents
        .iter()
        .filter(|d| d.path.as_path() != doc_path)
        .filter(|d| d.stem.to_lowercase() == new_stem)
        .map(|d| d.path.to_string())
        .collect();
    if !collisions.is_empty() {
        warnings.push(MutationWarning {
            code: "stem-collision".into(),
            field: None,
            message: format!("stem '{new_stem}' also at: {}", collisions.join(", ")),
        });
    }

    Ok(CreateBuilt {
        frontmatter: resolved_fm,
        created,
        warnings,
    })
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn parse_now(today: &str) -> anyhow::Result<NaiveDateTime> {
    if let Ok(dt) = NaiveDateTime::parse_from_str(today, "%Y-%m-%dT%H:%M:%S") {
        return Ok(dt);
    }
    let d = NaiveDate::parse_from_str(today, "%Y-%m-%d")
        .map_err(|e| anyhow::anyhow!("invalid today '{today}': {e}"))?;
    Ok(d.and_hms_opt(0, 0, 0).expect("midnight is always valid"))
}

/// A coded pre-write refusal. `code_owned` carries a dynamic code (containment /
/// coercion families) when the discriminator isn't a `'static` literal.
struct Refusal {
    code: &'static str,
    code_owned: Option<String>,
    message: String,
    path: Option<String>,
}

fn refusal(code: &'static str, message: impl Into<String>, path: Option<String>) -> Refusal {
    Refusal {
        code,
        code_owned: None,
        message: message.into(),
        path,
    }
}

fn refusal_owned(
    code: impl Into<String>,
    message: impl Into<String>,
    path: Option<String>,
) -> Refusal {
    Refusal {
        code: "",
        code_owned: Some(code.into()),
        message: message.into(),
        path,
    }
}

fn refused_new(r: Refusal) -> MutationExecution<NewReport> {
    let code = r.code_owned.unwrap_or_else(|| r.code.to_string());
    MutationExecution {
        report: NewReport {
            schema_version: 2,
            trace_id: String::new(),
            telemetry_degraded: false,
            operation: "new".into(),
            path: r.path.clone(),
            applied: false,
            outcome: MutationOutcome::Refused,
            frontmatter_created: Vec::new(),
            body_bytes: 0,
            warnings: Vec::new(),
            predicted_path: None,
            error: Some(CodedError {
                code,
                message: r.message,
                path: r.path,
            }),
        },
        touched_paths: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    const TODAY: &str = "2026-07-19";

    fn sink() -> crate::telemetry::EventSink {
        crate::telemetry::EventSink::discard(
            crate::telemetry::IdGen::with_seed(0),
            crate::telemetry::Clock::fixed("2026-07-19T00:00:00.000Z"),
        )
    }

    fn synth_vault(config: Option<&str>, docs: &[(&str, &str)]) -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        if let Some(cfg) = config {
            std::fs::create_dir(root.join(".norn").as_std_path()).unwrap();
            std::fs::write(root.join(".norn/config.yaml").as_std_path(), cfg).unwrap();
        }
        for (path, contents) in docs {
            let full = root.join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent.as_std_path()).unwrap();
            }
            std::fs::write(full.as_std_path(), contents).unwrap();
        }
        (tmp, root)
    }

    fn built(root: &Utf8PathBuf) -> crate::cache::Cache {
        let mut cache = crate::cache::Cache::open(root).unwrap();
        cache.full_build(root).unwrap();
        cache
    }

    fn parse_cfg(cfg: &str) -> VaultConfig {
        crate::standards::parse_config(cfg, camino::Utf8Path::new("c.yaml")).unwrap()
    }

    #[test]
    fn mode_a_explicit_path_applies() {
        let (_t, root) = synth_vault(None, &[]);
        let cache = built(&root);
        let params = NewParams {
            path: Some("notes/foo.md".into()),
            parents: true,
            confirm: true,
            ..Default::default()
        };
        let exec = execute(&cache, None, &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Applied);
        assert!(exec.report.applied);
        assert!(root.join("notes/foo.md").as_std_path().exists());
        assert!(!exec.touched_paths.is_empty());
    }

    #[test]
    fn mode_b_rule_target_resolves_and_defaults() {
        let cfg = r#"
validate:
  rules:
    - name: task
      target: "Workspaces/{{var.workspace}}/tasks/{{title|slugify}}.md"
      frontmatter_defaults:
        type: task
        status: backlog
"#;
        let (_t, root) = synth_vault(Some(cfg), &[]);
        let cache = built(&root);
        let config = parse_cfg(cfg);
        let params = NewParams {
            as_rule: Some("task".into()),
            title: Some("Fix It".into()),
            vars: vec!["workspace=norn".into()],
            parents: true,
            confirm: true,
            ..Default::default()
        };
        let exec = execute(&cache, Some(&config), &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Applied);
        assert_eq!(
            exec.report.path.as_deref(),
            Some("Workspaces/norn/tasks/fix-it.md")
        );
        assert!(root
            .join("Workspaces/norn/tasks/fix-it.md")
            .as_std_path()
            .exists());
        let by_field: BTreeMap<_, _> = exec
            .report
            .frontmatter_created
            .iter()
            .map(|c| (c.field.as_str(), c))
            .collect();
        assert_eq!(
            by_field.get("type").unwrap().value,
            Value::String("task".into())
        );
        assert_eq!(by_field.get("type").unwrap().source, "schema-default");
    }

    #[test]
    fn mode_c_inbox_requires_title_and_builds_path() {
        let cfg = "inbox:\n  path: Inbox\n";
        let (_t, root) = synth_vault(Some(cfg), &[]);
        let cache = built(&root);
        let config = parse_cfg(cfg);

        // Missing title → refused.
        let no_title = NewParams {
            confirm: false,
            ..Default::default()
        };
        let exec = execute(&cache, Some(&config), &no_title, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Refused);
        assert_eq!(
            exec.report.error.as_ref().unwrap().code,
            "inbox-requires-title"
        );

        // With title → path under the inbox.
        let params = NewParams {
            title: Some("My Title".into()),
            parents: true,
            confirm: true,
            ..Default::default()
        };
        let exec = execute(&cache, Some(&config), &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Applied);
        assert_eq!(exec.report.path.as_deref(), Some("Inbox/my-title.md"));
    }

    #[test]
    fn provenance_credits_override_and_default() {
        let cfg = r#"
validate:
  rules:
    - name: r
      match:
        path: "**/*.md"
      frontmatter_defaults:
        type: note
"#;
        let (_t, root) = synth_vault(Some(cfg), &[]);
        let cache = built(&root);
        let config = parse_cfg(cfg);
        let params = NewParams {
            path: Some("foo.md".into()),
            fields: vec!["title=My Note".into()],
            confirm: false,
            ..Default::default()
        };
        let exec = execute(&cache, Some(&config), &params, TODAY, &mut sink()).unwrap();
        let by_field: BTreeMap<_, _> = exec
            .report
            .frontmatter_created
            .iter()
            .map(|c| (c.field.as_str(), c))
            .collect();
        assert_eq!(by_field.get("type").unwrap().source, "schema-default");
        assert_eq!(by_field.get("type").unwrap().rule.as_deref(), Some("r"));
        assert_eq!(by_field.get("title").unwrap().source, "operator-flag");
        assert!(by_field.get("title").unwrap().rule.is_none());
    }

    #[test]
    fn f1_field_json_override_credits_operator_flag_json() {
        // The three-way source vocabulary: a --field-json override is
        // `operator-flag-json`, distinct from --field's `operator-flag`.
        let (_t, root) = synth_vault(None, &[]);
        let cache = built(&root);
        let params = NewParams {
            path: Some("foo.md".into()),
            fields: vec!["a=plain".into()],
            field_json: vec!["b=[1,2]".into()],
            confirm: false,
            ..Default::default()
        };
        let exec = execute(&cache, None, &params, TODAY, &mut sink()).unwrap();
        let by_field: BTreeMap<_, _> = exec
            .report
            .frontmatter_created
            .iter()
            .map(|c| (c.field.as_str(), c))
            .collect();
        assert_eq!(by_field.get("a").unwrap().source, "operator-flag");
        assert_eq!(by_field.get("b").unwrap().source, "operator-flag-json");
    }

    #[test]
    fn seq_target_forecast_sets_predicted_path() {
        let cfg = r#"
validate:
  rules:
    - name: task
      target: "tasks/MMR-{{seq}}.md"
      frontmatter_defaults:
        type: task
"#;
        let (_t, root) = synth_vault(Some(cfg), &[]);
        let cache = built(&root);
        let config = parse_cfg(cfg);
        let params = NewParams {
            as_rule: Some("task".into()),
            parents: true, // "tasks/" dir does not exist yet; forecast still previews
            confirm: false,
            ..Default::default()
        };
        let exec = execute(&cache, Some(&config), &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Forecast);
        assert!(!exec.report.applied);
        assert_eq!(
            exec.report.predicted_path.as_deref(),
            Some("tasks/MMR-1.md")
        );
        // The reported path stays the unresolved template on a forecast.
        assert_eq!(exec.report.path.as_deref(), Some("tasks/MMR-{{seq}}.md"));
    }

    #[test]
    fn destination_exists_refused_without_force() {
        let (_t, root) = synth_vault(None, &[("foo.md", "existing\n")]);
        let cache = built(&root);
        let params = NewParams {
            path: Some("foo.md".into()),
            confirm: false,
            ..Default::default()
        };
        let exec = execute(&cache, None, &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Refused);
        assert_eq!(
            exec.report.error.as_ref().unwrap().code,
            "destination-exists"
        );
    }

    #[test]
    fn force_overwrites_existing_destination() {
        let (_t, root) = synth_vault(None, &[("foo.md", "---\ntype: old\n---\nold body\n")]);
        let cache = built(&root);
        let params = NewParams {
            path: Some("foo.md".into()),
            force: true,
            confirm: true,
            ..Default::default()
        };
        let exec = execute(&cache, None, &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Applied);
        assert!(exec.report.applied);
        let on_disk = std::fs::read_to_string(root.join("foo.md").as_std_path()).unwrap();
        assert!(!on_disk.contains("old body"), "force should overwrite");
    }

    #[test]
    fn missing_var_refused() {
        let cfg = r#"
validate:
  rules:
    - name: task
      target: "Workspaces/{{var.workspace}}/tasks/{{title|slugify}}.md"
"#;
        let (_t, root) = synth_vault(Some(cfg), &[]);
        let cache = built(&root);
        let config = parse_cfg(cfg);
        let params = NewParams {
            as_rule: Some("task".into()),
            title: Some("X".into()),
            confirm: false,
            ..Default::default()
        };
        let exec = execute(&cache, Some(&config), &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Refused);
        assert_eq!(exec.report.error.as_ref().unwrap().code, "missing-var");
    }

    #[test]
    fn missing_title_refused() {
        let cfg = r#"
validate:
  rules:
    - name: note
      target: "notes/{{title|slugify}}.md"
"#;
        let (_t, root) = synth_vault(Some(cfg), &[]);
        let cache = built(&root);
        let config = parse_cfg(cfg);
        let params = NewParams {
            as_rule: Some("note".into()),
            confirm: false,
            ..Default::default()
        };
        let exec = execute(&cache, Some(&config), &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Refused);
        assert_eq!(exec.report.error.as_ref().unwrap().code, "missing-title");
    }

    #[test]
    fn body_from_stdin_lands_in_file() {
        let (_t, root) = synth_vault(None, &[]);
        let cache = built(&root);
        let params = NewParams {
            path: Some("notes/foo.md".into()),
            body: Some("# Hello\nbody\n".into()),
            parents: true,
            confirm: true,
            ..Default::default()
        };
        let exec = execute(&cache, None, &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Applied);
        // One trailing newline is trimmed.
        assert_eq!(exec.report.body_bytes, "# Hello\nbody".len());
        let on_disk = std::fs::read_to_string(root.join("notes/foo.md").as_std_path()).unwrap();
        assert!(on_disk.contains("# Hello"));
        assert!(on_disk.contains("body"));
    }

    #[test]
    fn path_and_rule_conflict_refused() {
        let (_t, root) = synth_vault(None, &[]);
        let cache = built(&root);
        let params = NewParams {
            path: Some("foo.md".into()),
            as_rule: Some("task".into()),
            confirm: false,
            ..Default::default()
        };
        let exec = execute(&cache, None, &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Refused);
        assert_eq!(
            exec.report.error.as_ref().unwrap().code,
            "path-and-rule-conflict"
        );
    }

    // ── NRN-390: post-create validate pass ──────────────────────────────────

    // ── NRN-430: allowed_values enforcement (refuse-preflight, --force bypass) ──

    const ALLOWED_CFG: &str = r#"
validate:
  rules:
    - name: r
      match:
        path: "**/*.md"
      allowed_values:
        status:
          - backlog
          - done
"#;

    #[test]
    fn disallowed_value_refuses_at_apply() {
        // `allowed_values` is a validate-engine rule `build_create`'s type
        // coercion never checked. Under NRN-430 a `--field status=someday`
        // violation REFUSES at preflight rather than warn-then-apply, writing
        // nothing. The refusal carries the allowed list so an agent can recover.
        let (_t, root) = synth_vault(Some(ALLOWED_CFG), &[]);
        let cache = built(&root);
        let config = parse_cfg(ALLOWED_CFG);
        let params = NewParams {
            path: Some("foo.md".into()),
            fields: vec!["status=someday".into()],
            confirm: true,
            ..Default::default()
        };
        let exec = execute(&cache, Some(&config), &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Refused);
        assert!(!exec.report.applied);
        assert!(
            !root.join("foo.md").as_std_path().exists(),
            "no file written"
        );
        let err = exec.report.error.as_ref().unwrap();
        assert_eq!(err.code, "value-not-allowed");
        assert!(err.message.contains("status"), "{}", err.message);
        // The allowed list is carried in the refusal message (not a separate
        // structured field), so an agent can recover without a second query.
        assert!(err.message.contains("backlog"), "{}", err.message);
        assert!(err.message.contains("done"), "{}", err.message);
        assert!(err.message.contains("--force"), "{}", err.message);
    }

    #[test]
    fn disallowed_value_refuses_at_forecast_defeating_plan_then_apply() {
        // The audit's defeating case: before NRN-430 a forecast never checked
        // allowed_values, so a clean forecast could precede a schema-violating
        // create. Now the forecast itself refuses — the plan is honest.
        let (_t, root) = synth_vault(Some(ALLOWED_CFG), &[]);
        let cache = built(&root);
        let config = parse_cfg(ALLOWED_CFG);
        let params = NewParams {
            path: Some("foo.md".into()),
            fields: vec!["status=someday".into()],
            confirm: false,
            ..Default::default()
        };
        let exec = execute(&cache, Some(&config), &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Refused);
        assert_eq!(
            exec.report.error.as_ref().unwrap().code,
            "value-not-allowed"
        );
    }

    #[test]
    fn disallowed_value_force_proceeds_with_bypass_warning() {
        // --force bypasses the refusal and applies, surfacing exactly one
        // force-bypass warning for the field (no duplicate value-not-allowed).
        let (_t, root) = synth_vault(Some(ALLOWED_CFG), &[]);
        let cache = built(&root);
        let config = parse_cfg(ALLOWED_CFG);
        let params = NewParams {
            path: Some("foo.md".into()),
            fields: vec!["status=someday".into()],
            force: true,
            confirm: true,
            ..Default::default()
        };
        let exec = execute(&cache, Some(&config), &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Applied);
        assert!(exec.report.applied);
        let bypass: Vec<_> = exec
            .report
            .warnings
            .iter()
            .filter(|w| w.code == "force-bypass" && w.field.as_deref() == Some("status"))
            .collect();
        assert_eq!(bypass.len(), 1, "one force-bypass warning for status");
        assert!(
            !exec
                .report
                .warnings
                .iter()
                .any(|w| w.code == "value-not-allowed"),
            "the post-create pass must not double the bypassed field as value-not-allowed"
        );
    }

    #[test]
    fn preflight_checks_every_co_applying_rule_not_just_the_first() {
        // Two co-applying rules declare DIFFERENT allowed_values for the same
        // field: a path-scoped rule and an unconditional (every-document) rule.
        // A value valid under the first-declared rule but rejected by the
        // second must still refuse — a preflight that stopped at the first
        // matching rule (via find_map) would let it through, surfacing the
        // violation only as the old post-apply warning (NRN-430 fix round).
        let cfg = r#"
validate:
  rules:
    - name: path-rule
      match:
        path: "tasks/**/*.md"
      allowed_values:
        status:
          - backlog
          - active
    - name: global-rule
      allowed_values:
        status:
          - backlog
          - done
"#;
        let (_t, root) = synth_vault(Some(cfg), &[]);
        let cache = built(&root);
        let config = parse_cfg(cfg);
        let params = NewParams {
            path: Some("tasks/foo.md".into()),
            fields: vec!["status=active".into()],
            parents: true,
            confirm: true,
            ..Default::default()
        };
        let exec = execute(&cache, Some(&config), &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Refused);
        assert!(!exec.report.applied);
        assert!(
            !root.join("tasks/foo.md").as_std_path().exists(),
            "no file written"
        );
        let err = exec.report.error.as_ref().unwrap();
        assert_eq!(err.code, "value-not-allowed");
        // The refusal names the SECOND (violated) rule's allowed set, not the
        // first rule's (which `active` does satisfy).
        assert!(err.message.contains("active"), "{}", err.message);
        assert!(
            err.message.contains("backlog") && err.message.contains("done"),
            "{}",
            err.message
        );
    }

    #[test]
    fn post_create_validate_no_findings_stays_clean() {
        // The happy path (no rules, nothing to violate) must stay warning-free
        // — the post-create pass is additive, not a source of noise.
        let (_t, root) = synth_vault(None, &[]);
        let cache = built(&root);
        let params = NewParams {
            path: Some("notes/foo.md".into()),
            parents: true,
            confirm: true,
            ..Default::default()
        };
        let exec = execute(&cache, None, &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Applied);
        assert!(exec.report.warnings.is_empty());
    }

    #[test]
    fn post_create_validate_does_not_duplicate_missing_required_warning() {
        // `build_create` already emits `missing-required-field` for a rule's own
        // `required_frontmatter` gap; the post-create validate pass's
        // `RequiredFrontmatterMissing` finding for the SAME field must be
        // deduplicated against it, not doubled.
        let cfg = r#"
validate:
  rules:
    - name: r
      match:
        path: "**/*.md"
      required_frontmatter:
        - status
"#;
        let (_t, root) = synth_vault(Some(cfg), &[]);
        let cache = built(&root);
        let config = parse_cfg(cfg);
        let params = NewParams {
            path: Some("foo.md".into()),
            confirm: true,
            ..Default::default()
        };
        let exec = execute(&cache, Some(&config), &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Applied);
        let missing_required: Vec<_> = exec
            .report
            .warnings
            .iter()
            .filter(|w| w.code == "missing-required-field" && w.field.as_deref() == Some("status"))
            .collect();
        assert_eq!(
            missing_required.len(),
            1,
            "missing-required-field for 'status' must appear exactly once, not duplicated \
             by the post-create validate pass"
        );
    }
}
