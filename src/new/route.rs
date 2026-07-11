//! CLI→service routing translation for `norn new` (NRN-230 PR C).
//!
//! `new` is the fifth routed mutation (after `set`/`edit` and the
//! `move`/`delete`/`rewrite-wikilink` cascade). It is routable byte-identically
//! because the `vault.new` MCP tool returns a `NewReport`-shaped
//! `structuredContent` (`{ "report": <NewReport>, ... }`, PR B) that this module
//! rebuilds into the native [`NewReport`] and renders through the SAME
//! `new::report::{render_records, render_json}` the direct path uses. So a routed
//! `norn new` and a direct one are byte-for-byte equal on stdout, stderr, and
//! exit code (the load-bearing isomorphism, ADR 0005).
//!
//! **Routable surface.** All three creation modes (explicit `path` / `--as RULE`
//! / inbox fallback), dry-run and apply. Unlike `set`, both `--field` AND
//! `--field-json` route: `vault.new`'s params carry them as ORDERED
//! `Vec<String>` token lists (not sorted maps), so passing the CLI's `Vec`s
//! through verbatim preserves last-wins/precedence exactly (audit F6). `--var`
//! is parsed CLI-side with the SAME `parse_var_args` the direct path uses and
//! shipped as the `vars` map (last-wins, order-immaterial).
//!
//! **Refusal rendering differs from `set` (audit F5).** The CLI `new` path has NO
//! JSON error envelope — a refusal is `error: {message}` prose on stderr + exit 2
//! in BOTH formats. `emit` therefore renders a coded wire refusal
//! format-INDEPENDENTLY as `error: {message}` + exit 2, byte-identical to the
//! direct arm's `eprintln!("error: {e}")`.
//!
//! **Gated to Direct** (see `try_route_new` in `src/lib.rs`): `--body-from-stdin`
//! (no wire-faithful stdin analogue), interactive TTY without `--yes` (lock
//! continuity across preview→prompt→apply has no routed equivalent), and
//! `--config` / `--no-cache-refresh` (the `routing_forced_direct` guard — the
//! daemon loads each vault's own default config and always serves a refreshed
//! warm cache).
//!
//! `to_mcp_arguments` / `reconstruct` / `emit` are pure so they unit-test without
//! a live daemon; the probe + wire round-trip live in the routing seam
//! (`src/lib.rs`).

use std::collections::BTreeMap;

use anyhow::Result;
use serde_json::{Map, Value};

use crate::apply_report::ApplyOutcome;
use crate::cli::{NewArgs, NewFormat};
use crate::new::report::NewReport;

/// Translate parsed `norn new` args into the `vault.new` tool's parameter object
/// (the `NewParams` shape in `src/mcp/tools/new.rs`), for the routable surface.
///
/// `vars` is the ALREADY-parsed `--var` map (`parse_var_args(&args.var)`): the
/// caller validates it CLI-side so a malformed `--var` refuses pre-send with the
/// direct path's exact prose. `confirm` is the dry-run/apply switch (false =
/// dry-run/preview, true = apply).
///
/// The caller (`try_route_new`) has already gated `--body-from-stdin`, so the
/// wire `body` param is deliberately absent here — a routed `new` never seeds an
/// explicit body.
pub fn to_mcp_arguments(args: &NewArgs, vars: &BTreeMap<String, String>, confirm: bool) -> Value {
    let mut map = Map::new();

    // Mode A: explicit path. Mode B: `--as RULE`. Mode C (inbox fallback): neither
    // — both omitted, the daemon resolves the inbox target from its config.
    if let Some(path) = &args.path {
        map.insert("path".into(), Value::String(path.as_str().to_string()));
    }
    if let Some(rule) = &args.as_rule {
        map.insert("rule".into(), Value::String(rule.clone()));
    }
    if let Some(title) = &args.title {
        map.insert("title".into(), Value::String(title.clone()));
    }
    if !vars.is_empty() {
        let obj: Map<String, Value> = vars
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        map.insert("vars".into(), Value::Object(obj));
    }
    // `--field` / `--field-json` ride the wire as ordered `Vec<String>` token
    // lists — the same lists the daemon's `build_plan` consumes last-wins.
    if !args.field.is_empty() {
        map.insert(
            "field".into(),
            Value::Array(args.field.iter().cloned().map(Value::String).collect()),
        );
    }
    if !args.field_json.is_empty() {
        map.insert(
            "field_json".into(),
            Value::Array(args.field_json.iter().cloned().map(Value::String).collect()),
        );
    }
    if args.parents {
        map.insert("parents".into(), Value::Bool(true));
    }
    if args.force {
        map.insert("force".into(), Value::Bool(true));
    }
    // `confirm` drives the MCP dry-run/apply contract; always sent so the wire is
    // explicit (the tool defaults it to false, but a routed apply must state it).
    map.insert("confirm".into(), Value::Bool(confirm));

    Value::Object(map)
}

/// Rebuild a [`NewReport`] from a `vault.new` `structuredContent` object.
///
/// The tool wraps the report under a `report` key (`NewOutput`), so this pulls
/// `structured["report"]` and deserializes it back into the native `NewReport` —
/// the exact inverse of the daemon's `serde_json::to_value(report)` projection,
/// so rendering the rebuilt value equals rendering the direct value (PR B made
/// `NewReport` serde-symmetric). A refused report MUST carry its `error`
/// envelope (the coded refusal `emit` renders); a missing one is a malformed
/// envelope, returned as `Err` so the seam handles it (fall back to Direct on a
/// dry-run, post-send-uncertain on an apply). Any shape mismatch is likewise an
/// `Err`.
pub fn reconstruct(structured: &Value) -> Result<NewReport> {
    let report_val = structured.get("report").ok_or_else(|| {
        anyhow::anyhow!("vault.new envelope: missing `report` object in structuredContent")
    })?;
    let report: NewReport = serde_json::from_value(report_val.clone())
        .map_err(|e| anyhow::anyhow!("vault.new envelope: unreadable report: {e}"))?;
    if matches!(report.outcome, ApplyOutcome::Refused) && report.error.is_none() {
        anyhow::bail!("vault.new envelope: refused report carries no `error` envelope");
    }
    Ok(report)
}

/// Render a reconstructed [`NewReport`] exactly as the direct `norn new` arm does,
/// returning the process exit code.
///
/// Three outcome families, each reproducing the direct path byte-for-byte:
///
/// - **refused** (a coded resolve/preflight/synth refusal): `new` has NO JSON
///   error envelope (audit F5) — reproduce the direct arm's `eprintln!("error:
///   {e}")` on stderr in BOTH formats and exit 2. The coded wire `error.message`
///   IS the `Display` of the same typed error the direct path prints.
/// - **applied** (a real `--yes` apply): render the report, then the records-only
///   `trace:` footer, and exit 0 — the same string the direct
///   `apply_and_render` builds in its `OutputBundle`.
/// - **dry-run / preview** (`applied == false`): render the report (which prints
///   the `Apply with --yes` hint), exit 0.
///
/// The direct `Command::New` arm writes its rendered string with `print!` (no
/// added trailing newline), so `emit` writes the reconstructed string the same
/// way — `render_json` carries no trailing newline, `render_records` ends in one.
pub fn emit(report: NewReport, format: NewFormat) -> Result<i32> {
    use std::io::Write as _;

    if matches!(report.outcome, ApplyOutcome::Refused) {
        // `reconstruct` guarantees `error` is present for a refused report.
        let error = report
            .error
            .as_ref()
            .expect("reconstruct guarantees a refused report carries `error`");
        // Byte-identical to the direct arm's `eprintln!("error: {e}")`, in BOTH
        // formats: `error.message` is the `Display` of the same typed refusal.
        eprintln!("error: {}", error.message);
        return Ok(2);
    }

    // Success / dry-run: rebuild the SAME rendered string the direct
    // `OutputBundle.rendered` carries. `render_records`/`render_json` key their
    // header/hint off `report.applied`, so one call reproduces both the apply and
    // the dry-run/preview shapes.
    let rendered = match format {
        NewFormat::Records => {
            let mut s = crate::new::report::render_records(&report);
            if report.applied {
                // The direct apply path appends a `trace:` footer after the
                // records block (records only; JSON carries `trace_id` as a field).
                s.push_str(&format!("trace: {}\n", report.trace_id));
            }
            s
        }
        NewFormat::Json => crate::new::report::render_json(&report)?,
    };

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    out.write_all(rendered.as_bytes())?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply_report::ApplyError;
    use crate::new::report::{FrontmatterCreated, NEW_REPORT_SCHEMA_VERSION};
    use crate::new::synth::{FieldSourceKind, Warning};
    use serde_json::json;

    fn base_args() -> NewArgs {
        NewArgs {
            path: None,
            as_rule: None,
            title: None,
            var: vec![],
            field: vec![],
            field_json: vec![],
            body_from_stdin: false,
            force: false,
            parents: false,
            yes: false,
            dry_run: false,
            format: NewFormat::Records,
        }
    }

    #[test]
    fn to_mcp_arguments_maps_mode_a_path() {
        let mut args = base_args();
        args.path = Some("notes/foo.md".into());
        let v = to_mcp_arguments(&args, &BTreeMap::new(), false);
        assert_eq!(v["path"], "notes/foo.md");
        assert!(v.get("rule").is_none(), "Mode A carries no rule");
        assert_eq!(v["confirm"], false);
    }

    #[test]
    fn to_mcp_arguments_maps_mode_b_rule_title_vars() {
        let mut args = base_args();
        args.as_rule = Some("task".into());
        args.title = Some("Fix It".into());
        let mut vars = BTreeMap::new();
        vars.insert("workspace".to_string(), "norn".to_string());
        let v = to_mcp_arguments(&args, &vars, true);
        assert_eq!(v["rule"], "task");
        assert_eq!(v["title"], "Fix It");
        assert_eq!(v["vars"], json!({ "workspace": "norn" }));
        assert!(v.get("path").is_none(), "Mode B carries no path");
        assert_eq!(v["confirm"], true);
    }

    #[test]
    fn to_mcp_arguments_maps_field_field_json_parents_force() {
        let mut args = base_args();
        args.path = Some("foo.md".into());
        // Ordered, last-wins-preserving token lists ride verbatim.
        args.field = vec!["status=done".into(), "status=active".into()];
        args.field_json = vec!["tags=[\"a\"]".into()];
        args.parents = true;
        args.force = true;
        let v = to_mcp_arguments(&args, &BTreeMap::new(), true);
        assert_eq!(v["field"], json!(["status=done", "status=active"]));
        assert_eq!(v["field_json"], json!(["tags=[\"a\"]"]));
        assert_eq!(v["parents"], true);
        assert_eq!(v["force"], true);
    }

    #[test]
    fn to_mcp_arguments_omits_empty_optionals() {
        // Mode C (inbox fallback): no path, no rule — both omitted.
        let mut args = base_args();
        args.title = Some("A Title".into());
        let v = to_mcp_arguments(&args, &BTreeMap::new(), false);
        assert!(v.get("path").is_none());
        assert!(v.get("rule").is_none());
        assert!(v.get("vars").is_none(), "empty vars must be omitted");
        assert!(v.get("field").is_none(), "empty field must be omitted");
        assert!(v.get("field_json").is_none());
        assert!(v.get("parents").is_none(), "parents:false must be omitted");
        assert!(v.get("force").is_none(), "force:false must be omitted");
        assert_eq!(v["title"], "A Title");
        // confirm is always explicit.
        assert_eq!(v["confirm"], false);
    }

    fn applied_report(applied: bool, trace: &str) -> NewReport {
        NewReport {
            schema_version: NEW_REPORT_SCHEMA_VERSION,
            operation: "new".to_string(),
            path: Some("notes/foo.md".to_string()),
            applied,
            outcome: ApplyOutcome::Applied,
            trace_id: trace.to_string(),
            frontmatter_created: vec![FrontmatterCreated {
                field: "type".to_string(),
                value: json!("note"),
                source: FieldSourceKind::SchemaDefault,
                rule: Some("any".to_string()),
            }],
            body_bytes: 0,
            warnings: vec![Warning::TitleIgnored {
                title: "X".to_string(),
            }],
            predicted_path: None,
            error: None,
        }
    }

    /// Project a `NewReport` to the wire (`{ "report": <report> }`, the
    /// `NewOutput` shape) and rebuild it: the reconstruction is the exact
    /// inverse, so the rebuilt value renders byte-identically in both formats.
    fn assert_round_trip(report: NewReport) {
        let wire = json!({ "report": serde_json::to_value(&report).unwrap() });
        let rebuilt = reconstruct(&wire).unwrap();
        // render_json byte-identity.
        assert_eq!(
            crate::new::report::render_json(&report).unwrap(),
            crate::new::report::render_json(&rebuilt).unwrap(),
            "render_json must match across the round-trip"
        );
        // render_records byte-identity.
        assert_eq!(
            crate::new::report::render_records(&report),
            crate::new::report::render_records(&rebuilt),
            "render_records must match across the round-trip"
        );
    }

    #[test]
    fn round_trip_dry_run_report() {
        assert_round_trip(applied_report(false, ""));
    }

    #[test]
    fn round_trip_applied_report() {
        assert_round_trip(applied_report(true, "abc123"));
    }

    /// A refused report round-trips its coded `error` envelope so `emit` can
    /// reproduce the direct refusal output.
    #[test]
    fn round_trip_refused_report_preserves_error() {
        let report = NewReport::refused(ApplyError {
            code: "destination-exists".to_string(),
            message: "destination already exists (use --force to overwrite): exists.md".to_string(),
            path: Some("exists.md".to_string()),
        });
        let wire = json!({ "report": serde_json::to_value(&report).unwrap() });
        let rebuilt = reconstruct(&wire).unwrap();
        assert!(matches!(rebuilt.outcome, ApplyOutcome::Refused));
        let err = rebuilt
            .error
            .expect("refused report keeps its error envelope");
        assert_eq!(err.code, "destination-exists");
    }

    /// A forwarded-note envelope (NRN-215): the daemon may inject an
    /// `operator_notes` sibling ALONGSIDE the `report` key. `reconstruct` reads
    /// only `report`, so the extra sibling never corrupts the rebuilt report.
    #[test]
    fn reconstruct_ignores_operator_notes_sibling() {
        let report = applied_report(false, "");
        let mut wire = json!({ "report": serde_json::to_value(&report).unwrap() });
        wire.as_object_mut().unwrap().insert(
            "operator_notes".into(),
            json!(["vault: another cache operation is in progress; using current cache state"]),
        );
        let rebuilt = reconstruct(&wire).unwrap();
        assert_eq!(
            crate::new::report::render_records(&report),
            crate::new::report::render_records(&rebuilt),
            "the notes sibling must not affect the rebuilt report"
        );
    }

    /// A refused envelope missing its `error` is malformed — `reconstruct` errs so
    /// the seam handles it (fall back on dry-run, post-send-uncertain on apply),
    /// rather than panicking in `emit`.
    #[test]
    fn reconstruct_refused_without_error_is_err() {
        let mut report = applied_report(false, "");
        report.outcome = ApplyOutcome::Refused;
        report.error = None;
        let wire = json!({ "report": serde_json::to_value(&report).unwrap() });
        assert!(reconstruct(&wire).is_err());
    }

    #[test]
    fn reconstruct_missing_report_key_is_err() {
        assert!(reconstruct(&json!({ "not_report": {} })).is_err());
    }

    /// `emit` on a refused report exits 2 (format-independently — `new` has no
    /// JSON error envelope).
    #[test]
    fn emit_refused_exits_two_in_both_formats() {
        for fmt in [NewFormat::Records, NewFormat::Json] {
            let report = NewReport::refused(ApplyError {
                code: "unknown-rule".to_string(),
                message: "unknown rule `bogus`".to_string(),
                path: None,
            });
            let code = emit(report, fmt).unwrap();
            assert_eq!(code, 2, "refusal exits 2 for {fmt:?}");
        }
    }
}
