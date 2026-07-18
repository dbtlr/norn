//! CLI→service routing translation for `norn set` (NRN-229).
//!
//! `set` is the FIRST routed mutation (the read siblings are `count`/`find`/`get`).
//! It is routable byte-identically because the `vault.set` MCP tool returns a
//! `SetReport`-shaped `structuredContent` (`{ "report": <SetReport>, ... }`) that
//! this module rebuilds into the native [`SetReport`] and renders through the
//! SAME `set::report::{render_records, render_json}` — and the same refusal
//! rendering (`render_json_error_envelope` / `error:`-prose) — the direct path
//! uses. So a routed `norn set` and a direct one are byte-for-byte equal on
//! stdout, stderr, and exit code (the load-bearing isomorphism, ADR 0005).
//!
//! **Routable surface.** Only the shapes that map onto the wire without
//! reordering or collapsing are routed; the send-commit policy + confirm mapping
//! and the shape gating live in `route_set`/`try_route_set` (`src/lib.rs`):
//!
//! - Routed: `target`, `--field` / trailing `KEY=VALUE` positionals (folded into
//!   the tool's `field` list in the SAME order `desugar_positional_fields`
//!   produces), `--field-json`, `--push`, `--pop` (NRN-238: `vault.set`'s params
//!   carry these as ORDERED `Vec<String>` token lists — not sorted maps — so the
//!   CLI's argv-ordered `Vec`s ride the wire verbatim, preserving order and
//!   duplicate-key-accumulate semantics exactly, the same way `vault.new`'s
//!   `field`/`field_json` already do), `--remove`, `--force`.
//! - Gated to Direct: `--body-from-stdin` (no wire-faithful stdin analogue — an
//!   MCP server has no stdin).
//!
//! `to_mcp_arguments` / `reconstruct` / `emit` are pure so they unit-test without
//! a live daemon; the probe + wire round-trip live in the routing seam
//! (`src/lib.rs`).

use anyhow::Result;
use serde_json::{Map, Value};

use crate::apply_report::ApplyOutcome;
use crate::cli::{SetArgs, SetFormat};
use crate::set::report::SetReport;

/// Translate parsed `norn set` args into the `vault.set` tool's parameter object
/// (the `SetParams` shape in `src/mcp/tools/set.rs`), for the routable surface.
///
/// `combined_fields` is the ALREADY-desugared `--field` list
/// (`desugar_positional_fields(field_pos, fields)`): the daemon sets
/// `field_pos = []` and folds positionals into `field` at preflight, so the wire
/// must carry the pre-combined list in the identical order. `confirm` is the
/// dry-run/apply switch (false = dry-run/preview, true = apply).
///
/// `--field-json` / `--push` / `--pop` ride the wire as ordered `Vec<String>`
/// token lists — the SAME lists the daemon's `set::synth`/`validate` seam
/// consumes — mirroring `vault.new`'s `field`/`field_json` (NRN-238). The caller
/// (`route_set`) has already gated `--body-from-stdin` (no wire-faithful stdin
/// analogue), so that field is deliberately absent here.
pub fn to_mcp_arguments(args: &SetArgs, combined_fields: &[String], confirm: bool) -> Value {
    let mut map = Map::new();
    map.insert("target".into(), Value::String(args.target.clone()));

    if !combined_fields.is_empty() {
        map.insert(
            "field".into(),
            Value::Array(combined_fields.iter().cloned().map(Value::String).collect()),
        );
    }
    if !args.field_json.is_empty() {
        map.insert(
            "field_json".into(),
            Value::Array(args.field_json.iter().cloned().map(Value::String).collect()),
        );
    }
    if !args.push.is_empty() {
        map.insert(
            "push".into(),
            Value::Array(args.push.iter().cloned().map(Value::String).collect()),
        );
    }
    if !args.pop.is_empty() {
        map.insert(
            "pop".into(),
            Value::Array(args.pop.iter().cloned().map(Value::String).collect()),
        );
    }
    if !args.remove.is_empty() {
        map.insert(
            "remove".into(),
            Value::Array(args.remove.iter().cloned().map(Value::String).collect()),
        );
    }
    if args.force {
        map.insert("force".into(), Value::Bool(true));
    }
    // `confirm` drives the MCP dry-run/apply contract; always sent so the wire is
    // explicit (the tool defaults it to false, but a routed apply must state it).
    map.insert("confirm".into(), Value::Bool(confirm));

    Value::Object(map)
}

/// Rebuild a [`SetReport`] from a `vault.set` `structuredContent` object.
///
/// The tool wraps the report under a `report` key (`SetOutput`), so this pulls
/// `structured["report"]` and deserializes it back into the native `SetReport` —
/// the exact inverse of the daemon's `serde_json::to_value(report)` projection,
/// so rendering the rebuilt value equals rendering the direct value. A refused
/// report MUST carry its `error` envelope (the coded refusal `emit` renders); a
/// missing one is a malformed envelope, returned as `Err` so the seam handles it
/// (fall back to Direct on a dry-run, post-send-uncertain on an apply). Any shape
/// mismatch is likewise an `Err`.
pub fn reconstruct(structured: &Value) -> Result<SetReport> {
    let report_val = structured.get("report").ok_or_else(|| {
        anyhow::anyhow!("vault.set envelope: missing `report` object in structuredContent")
    })?;
    let report: SetReport = serde_json::from_value(report_val.clone())
        .map_err(|e| anyhow::anyhow!("vault.set envelope: unreadable report: {e}"))?;
    if matches!(report.outcome, ApplyOutcome::Refused) && report.error.is_none() {
        anyhow::bail!("vault.set envelope: refused report carries no `error` envelope");
    }
    Ok(report)
}

/// Render a reconstructed [`SetReport`] exactly as the direct `norn set` arm does,
/// returning the process exit code.
///
/// Three outcome families, each reproducing the direct path byte-for-byte:
///
/// - **refused** (a coded schema/argument/target refusal, NRN-221): reproduce the
///   direct preflight-refusal output — the pretty `ApplyError` envelope on stdout
///   for `--format json` (matching `render_json_error_envelope`), or `error:`
///   prose on stderr for records — and exit 2.
/// - **applied** (a real `--yes` apply): render the report, then the records-only
///   `trace:` footer, and exit 0.
/// - **dry-run / preview** (`applied == false`, outcome `applied`): render the
///   report (which prints the `dry-run: set …` header + `Apply with --yes` hint),
///   exit 0.
pub fn emit(report: SetReport, format: SetFormat) -> Result<i32> {
    use std::io::Write as _;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    if matches!(report.outcome, ApplyOutcome::Refused) {
        // `reconstruct` guarantees `error` is present for a refused report.
        let error = report
            .error
            .as_ref()
            .expect("reconstruct guarantees a refused report carries `error`");
        match format {
            SetFormat::Json => {
                // Byte-identical to `render_json_error_envelope`: the report's
                // `error` IS the `ApplyError` that `ApplyError::from_anyhow`
                // produces for the same `SetError` on the direct path.
                let json = serde_json::to_string_pretty(error)?;
                out.write_all(json.as_bytes())?;
                out.write_all(b"\n")?;
            }
            SetFormat::Records => {
                // Byte-identical to the direct arm's `eprintln!("error: {e}")`:
                // `error.message` is the `SetError` Display.
                eprintln!("error: {}", error.message);
            }
        }
        return Ok(2);
    }

    // Applied, or a dry-run/preview forecast — the report renderers key their
    // header/hint off `report.applied`, so one call reproduces both.
    match format {
        SetFormat::Records => {
            crate::set::report::render_records(&mut out, &report)?;
            if report.applied {
                // The direct apply path prints a `trace:` footer after the records
                // block (records only; JSON carries `trace_id` as a field).
                writeln!(out, "trace: {}", report.trace_id)?;
            }
        }
        SetFormat::Json => {
            crate::set::report::render_json(&mut out, &report)?;
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply_report::ApplyError;
    use crate::set::report::{FrontmatterChange, SetReport, SET_REPORT_SCHEMA_VERSION};
    use serde_json::json;

    fn base_args() -> SetArgs {
        SetArgs {
            target: "task".into(),
            fields: vec![],
            field_pos: vec![],
            field_json: vec![],
            push: vec![],
            pop: vec![],
            remove: vec![],
            body_from_stdin: false,
            force: false,
            yes: false,
            dry_run: false,
            format: SetFormat::Records,
        }
    }

    #[test]
    fn to_mcp_arguments_maps_target_fields_remove_force_confirm() {
        let mut args = base_args();
        args.remove = vec!["stale".into()];
        args.force = true;
        let combined = vec!["status=done".to_string(), "tags=x".to_string()];

        let v = to_mcp_arguments(&args, &combined, true);
        assert_eq!(v["target"], "task");
        assert_eq!(v["field"], json!(["status=done", "tags=x"]));
        assert_eq!(v["remove"], json!(["stale"]));
        assert_eq!(v["force"], true);
        assert_eq!(v["confirm"], true);
    }

    /// NRN-238: `--field-json` / `--push` / `--pop` ride the wire as ordered
    /// `Vec<String>` token lists, verbatim — including a duplicate key across
    /// tokens, which a sorted map could never express (the load-bearing reason
    /// these three now route instead of gating to Direct).
    #[test]
    fn to_mcp_arguments_maps_field_json_push_pop_verbatim_with_duplicate_keys() {
        let mut args = base_args();
        args.field_json = vec!["tags=[\"a\"]".into(), "status=\"done\"".into()];
        args.push = vec!["tags=gamma".into(), "tags=delta".into()];
        args.pop = vec!["tags=alpha".into()];

        let v = to_mcp_arguments(&args, &[], true);
        assert_eq!(
            v["field_json"],
            json!(["tags=[\"a\"]", "status=\"done\""]),
            "field_json tokens must ride the wire in order, verbatim"
        );
        assert_eq!(
            v["push"],
            json!(["tags=gamma", "tags=delta"]),
            "a duplicate push key across tokens must ride the wire as two ordered entries"
        );
        assert_eq!(v["pop"], json!(["tags=alpha"]));
    }

    /// Omit-when-empty: an empty `field_json`/`push`/`pop` must not appear in the
    /// wire object at all, mirroring `field`/`remove`/`force` (and `vault.new`'s
    /// `field`/`field_json` omission, `src/new/route.rs`).
    #[test]
    fn to_mcp_arguments_omits_empty_field_json_push_pop() {
        let v = to_mcp_arguments(&base_args(), &[], false);
        assert!(
            v.get("field_json").is_none(),
            "empty field_json list must be omitted"
        );
        assert!(v.get("push").is_none(), "empty push list must be omitted");
        assert!(v.get("pop").is_none(), "empty pop list must be omitted");
    }

    #[test]
    fn to_mcp_arguments_omits_empty_field_and_remove_and_force() {
        let v = to_mcp_arguments(&base_args(), &[], false);
        assert_eq!(v["target"], "task");
        assert!(v.get("field").is_none(), "empty field list must be omitted");
        assert!(
            v.get("remove").is_none(),
            "empty remove list must be omitted"
        );
        assert!(v.get("force").is_none(), "force:false must be omitted");
        // confirm is always explicit.
        assert_eq!(v["confirm"], false);
    }

    /// Project a `SetReport` to the wire (`{ "report": <report> }`, the `SetOutput`
    /// shape) and rebuild it: the reconstruction is the exact inverse, so the
    /// rebuilt value renders byte-identically in both formats.
    fn assert_round_trip(report: SetReport) {
        let wire = json!({ "report": serde_json::to_value(&report).unwrap() });
        let rebuilt = reconstruct(&wire).unwrap();
        for fmt in [SetFormat::Records, SetFormat::Json] {
            let mut a = Vec::new();
            let mut b = Vec::new();
            // Render the ORIGINAL and the REBUILT through the same renderer.
            if fmt == SetFormat::Json {
                crate::set::report::render_json(&mut a, &report).unwrap();
                crate::set::report::render_json(&mut b, &rebuilt).unwrap();
            } else {
                crate::set::report::render_records(&mut a, &report).unwrap();
                crate::set::report::render_records(&mut b, &rebuilt).unwrap();
            }
            assert_eq!(a, b, "rendered bytes must match for {fmt:?}");
        }
    }

    fn applied_report(applied: bool, trace: &str) -> SetReport {
        SetReport {
            schema_version: SET_REPORT_SCHEMA_VERSION,
            trace_id: trace.to_string(),
            operation: "set".to_string(),
            target: "notes/task.md".into(),
            frontmatter_changes: vec![FrontmatterChange {
                op: "set".to_string(),
                field: "status".to_string(),
                old: Some(json!("backlog")),
                new: Some(json!("done")),
                value: None,
                found: None,
            }],
            body_changed: false,
            body_bytes_new: None,
            body_bytes_old: None,
            applied,
            outcome: ApplyOutcome::Applied,
            error: None,
            warnings: vec![],
        }
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
        let report = SetReport::refused(
            "notes/task.md".into(),
            ApplyError {
                code: "value-not-allowed".to_string(),
                message: "value 'bogus' is not allowed for 'status' (allowed: backlog, done)"
                    .to_string(),
                path: None,
            },
        );
        let wire = json!({ "report": serde_json::to_value(&report).unwrap() });
        let rebuilt = reconstruct(&wire).unwrap();
        assert!(matches!(rebuilt.outcome, ApplyOutcome::Refused));
        let err = rebuilt
            .error
            .expect("refused report keeps its error envelope");
        assert_eq!(err.code, "value-not-allowed");
    }

    /// A forwarded-note envelope (NRN-215): the daemon injects an `operator_notes`
    /// sibling ALONGSIDE the `report` key. `reconstruct` reads only `report`, so
    /// the extra sibling never corrupts the rebuilt `SetReport` — the note rides
    /// on for the routing seam to re-emit on stderr.
    #[test]
    fn reconstruct_ignores_operator_notes_sibling() {
        let report = applied_report(false, "");
        let mut wire = json!({ "report": serde_json::to_value(&report).unwrap() });
        wire.as_object_mut().unwrap().insert(
            "operator_notes".into(),
            json!(["vault: another cache operation is in progress; using current cache state"]),
        );
        let rebuilt = reconstruct(&wire).unwrap();
        let mut a = Vec::new();
        let mut b = Vec::new();
        crate::set::report::render_records(&mut a, &report).unwrap();
        crate::set::report::render_records(&mut b, &rebuilt).unwrap();
        assert_eq!(a, b, "the notes sibling must not affect the rebuilt report");
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

    /// `emit` on a refused report exits 2 and writes the pretty `ApplyError`
    /// envelope for `--format json` (matching `render_json_error_envelope`).
    #[test]
    fn emit_refused_json_exits_two() {
        let report = SetReport::refused(
            "notes/task.md".into(),
            ApplyError {
                code: "target-not-found".to_string(),
                message: "doc not found: nope".to_string(),
                path: None,
            },
        );
        let code = emit(report, SetFormat::Json).unwrap();
        assert_eq!(code, 2);
    }
}
