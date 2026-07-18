//! CLI→service routing translation for `norn edit` (NRN-229 PR A).
//!
//! `edit` is the second routed mutation, copying the `set` template
//! (`src/set/route.rs`) exactly: it is routable byte-identically because the
//! `vault.edit` MCP tool returns an `EditReport`-shaped `structuredContent`
//! (`{ "report": <EditReport>, ... }`) that this module rebuilds into the
//! native [`EditReport`] and renders through the SAME
//! `edit::report::{render_records, render_json}` the direct path uses. So a
//! routed `norn edit` and a direct one are byte-for-byte equal on stdout,
//! stderr, and exit code (the load-bearing isomorphism, ADR 0005) — with one
//! deliberate, pre-existing exception: see [`emit`]'s refusal branch.
//!
//! **Routable surface.** Unlike `set`, `edit`'s ops arrive as an already-parsed
//! `Vec<EditOp>` — resolved LOCALLY by the direct arm before routing is
//! attempted, whether they came from `--edits-json`, `--ops-file`, stdin, or
//! single-op sugar desugaring (`edit::sugar::desugar`). Routing re-serializes
//! that resolved `Vec<EditOp>` onto the wire, so every source is routable; the
//! source itself is never a reason to gate Direct (unlike `set`'s
//! `--body-from-stdin`, which ships raw, unparsed body bytes).
//!
//! - Routed: `target`, the resolved `edits` array, `--expected-hash`.
//! - Gated to Direct: nothing today — every `EditArgs` field that reaches
//!   `edit::synth::preflight_and_plan` (target, ops, expected_hash) has a
//!   faithful wire encoding in `vault.edit`'s `EditParams`. The mode-mapping
//!   flags (`--yes` / `--dry-run` / `--format`) are consumed by the confirm
//!   ladder in `try_route_edit` (`src/lib.rs`), not carried onto the wire.
//!
//! `to_mcp_arguments` / `reconstruct` / `emit` are pure so they unit-test
//! without a live daemon; the probe + wire round-trip live in the routing seam
//! (`src/lib.rs`).

use anyhow::Result;
use serde_json::{Map, Value};

use crate::apply_report::ApplyOutcome;
use crate::cli::{EditArgs, EditFormat};
use crate::edit::ops::EditOp;
use crate::edit::report::EditReport;

/// Translate parsed `norn edit` args + the locally-resolved ops into the
/// `vault.edit` tool's parameter object (the `EditParams` shape in
/// `src/mcp/tools/edit.rs`).
///
/// `ops` is whatever the direct arm already resolved (sugar-desugared, or
/// parsed from `--edits-json`/`--ops-file`/stdin) — the wire carries the
/// SAME resolved array, in the SAME order, so `edit::transform::apply_edits`
/// runs identically on both paths. `confirm` is the dry-run/apply switch
/// (false = dry-run/preview, true = apply).
pub fn to_mcp_arguments(args: &EditArgs, ops: &[EditOp], confirm: bool) -> Value {
    let mut map = Map::new();
    map.insert("target".into(), Value::String(args.target.clone()));
    map.insert(
        "edits".into(),
        Value::Array(
            ops.iter()
                .map(|op| {
                    serde_json::to_value(op)
                        .expect("EditOp has no non-string map keys or NaN floats; cannot fail")
                })
                .collect(),
        ),
    );
    if let Some(hash) = &args.expected_hash {
        map.insert("expected_hash".into(), Value::String(hash.clone()));
    }
    // `confirm` drives the MCP dry-run/apply contract; always sent so the wire
    // is explicit (the tool defaults it to false, but a routed apply must state
    // it).
    map.insert("confirm".into(), Value::Bool(confirm));

    Value::Object(map)
}

/// Rebuild an [`EditReport`] from a `vault.edit` `structuredContent` object.
///
/// The tool wraps the report under a `report` key (`EditOutput`), so this
/// pulls `structured["report"]` and deserializes it back into the native
/// `EditReport` — the exact inverse of the daemon's
/// `serde_json::to_value(report)` projection, so rendering the rebuilt value
/// equals rendering the direct value. A refused report MUST carry its `error`
/// envelope (the coded refusal `emit` renders); a missing one is a malformed
/// envelope, returned as `Err` so the seam handles it (fall back to Direct on
/// a dry-run, post-send-uncertain on an apply). Any shape mismatch is likewise
/// an `Err`.
pub fn reconstruct(structured: &Value) -> Result<EditReport> {
    let report_val = structured.get("report").ok_or_else(|| {
        anyhow::anyhow!("vault.edit envelope: missing `report` object in structuredContent")
    })?;
    let report: EditReport = serde_json::from_value(report_val.clone())
        .map_err(|e| anyhow::anyhow!("vault.edit envelope: unreadable report: {e}"))?;
    if matches!(report.outcome, ApplyOutcome::Refused) && report.error.is_none() {
        anyhow::bail!("vault.edit envelope: refused report carries no `error` envelope");
    }
    Ok(report)
}

/// Render a reconstructed [`EditReport`] exactly as the direct `norn edit` arm
/// does, returning the process exit code.
///
/// Three outcome families:
///
/// - **refused** (a coded schema/anchor/target refusal, NRN-220): the direct
///   `Command::Edit` arm's preflight-refusal branch (`src/lib.rs`) does NOT
///   branch on `--format` — unlike `set`/`move`/`delete` (NRN-221), it always
///   `eprintln!("error: {e}")` and exits 2, for records AND json alike. This is
///   a pre-existing asymmetry in the direct path, not something this routing
///   seam may fix (byte-identical-to-Direct outranks internal consistency
///   here) — so `emit` reproduces it verbatim: prose on stderr, exit 2,
///   regardless of `format`. `error.message` is the same `Display` string
///   Direct's `{e}` interpolates (both trace back to the same underlying
///   `EditError`/`SetError`), so the two are byte-identical.
/// - **applied** (a real `--yes` apply): render the report, then the
///   records-only `trace:` footer, and exit 0.
/// - **dry-run / preview** (`applied == false`, outcome `applied`): render the
///   report (which prints the `dry-run: edit …` header + `Apply with --yes`
///   hint), exit 0.
pub fn emit(report: EditReport, format: EditFormat) -> Result<i32> {
    use std::io::Write as _;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    if matches!(report.outcome, ApplyOutcome::Refused) {
        // `reconstruct` guarantees `error` is present for a refused report.
        let error = report
            .error
            .as_ref()
            .expect("reconstruct guarantees a refused report carries `error`");
        eprintln!("error: {}", error.message);
        return Ok(2);
    }

    // Applied, or a dry-run/preview forecast — the report renderers key their
    // header/hint off `report.applied`, so one call reproduces both.
    match format {
        EditFormat::Records => {
            crate::edit::report::render_records(&mut out, &report)?;
            if report.applied {
                // The direct apply path prints a `trace:` footer after the
                // records block (records only; JSON carries `trace_id` as a
                // field).
                writeln!(out, "trace: {}", report.trace_id)?;
            }
        }
        EditFormat::Json => {
            crate::edit::report::render_json(&mut out, &report)?;
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply_report::ApplyError;
    use crate::edit::report::EditChange;
    use serde_json::json;

    fn base_args() -> EditArgs {
        EditArgs {
            target: "note".into(),
            edits_json: None,
            ops_file: None,
            str_replace: None,
            replace_section: None,
            append_to_section: None,
            delete_section: None,
            insert_before_heading: None,
            insert_after_heading: None,
            new: None,
            content: None,
            replace_all: false,
            expected_hash: None,
            yes: false,
            dry_run: false,
            format: EditFormat::Records,
        }
    }

    fn str_replace_op(old: &str, new: &str) -> EditOp {
        EditOp::StrReplace {
            old: old.to_string(),
            new: new.to_string(),
            replace_all: false,
        }
    }

    #[test]
    fn to_mcp_arguments_maps_target_edits_hash_confirm() {
        let mut args = base_args();
        args.expected_hash = Some("deadbeef".into());
        let ops = vec![str_replace_op("old", "new")];

        let v = to_mcp_arguments(&args, &ops, true);
        assert_eq!(v["target"], "note");
        assert_eq!(
            v["edits"],
            json!([{"op": "str_replace", "old": "old", "new": "new", "replace_all": false}])
        );
        assert_eq!(v["expected_hash"], "deadbeef");
        assert_eq!(v["confirm"], true);
    }

    #[test]
    fn to_mcp_arguments_omits_absent_expected_hash() {
        let ops = vec![str_replace_op("old", "new")];
        let v = to_mcp_arguments(&base_args(), &ops, false);
        assert_eq!(v["target"], "note");
        assert!(
            v.get("expected_hash").is_none(),
            "absent --expected-hash must be omitted"
        );
        // confirm is always explicit.
        assert_eq!(v["confirm"], false);
    }

    /// Preserves multi-op order on the wire — `apply_edits` runs each op
    /// against the result of the prior, so order is load-bearing.
    #[test]
    fn to_mcp_arguments_preserves_multi_op_order() {
        let ops = vec![
            str_replace_op("a", "b"),
            EditOp::DeleteSection {
                heading: "Old".into(),
            },
        ];
        let v = to_mcp_arguments(&base_args(), &ops, true);
        let edits = v["edits"].as_array().expect("edits array");
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0]["op"], "str_replace");
        assert_eq!(edits[1]["op"], "delete_section");
    }

    /// Project an `EditReport` to the wire (`{ "report": <report> }`, the
    /// `EditOutput` shape) and rebuild it: the reconstruction is the exact
    /// inverse, so the rebuilt value renders byte-identically in both formats.
    fn assert_round_trip(report: EditReport) {
        let wire = json!({ "report": serde_json::to_value(&report).unwrap() });
        let rebuilt = reconstruct(&wire).unwrap();
        for fmt in [EditFormat::Records, EditFormat::Json] {
            let mut a = Vec::new();
            let mut b = Vec::new();
            if fmt == EditFormat::Json {
                crate::edit::report::render_json(&mut a, &report).unwrap();
                crate::edit::report::render_json(&mut b, &rebuilt).unwrap();
            } else {
                crate::edit::report::render_records(&mut a, &report).unwrap();
                crate::edit::report::render_records(&mut b, &rebuilt).unwrap();
            }
            assert_eq!(a, b, "rendered bytes must match for {fmt:?}");
        }
    }

    fn applied_report(applied: bool, trace: &str) -> EditReport {
        EditReport {
            schema_version: crate::edit::report::SCHEMA_VERSION,
            trace_id: trace.to_string(),
            operation: "edit".to_string(),
            target: "notes/note.md".into(),
            edits: vec![EditChange {
                op: "str_replace".to_string(),
                anchor: r#"old="old""#.to_string(),
                matched: true,
                occurrences: Some(1),
                applied,
            }],
            body_changed: true,
            body_bytes_old: Some(10),
            body_bytes_new: Some(12),
            applied,
            outcome: ApplyOutcome::Applied,
            error: None,
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
        let report = EditReport::refused(
            "notes/note.md".into(),
            ApplyError {
                code: "anchor-not-found".to_string(),
                message: "anchor not found: old=\"nope\"".to_string(),
                path: None,
            },
        );
        let wire = json!({ "report": serde_json::to_value(&report).unwrap() });
        let rebuilt = reconstruct(&wire).unwrap();
        assert!(matches!(rebuilt.outcome, ApplyOutcome::Refused));
        let err = rebuilt
            .error
            .expect("refused report keeps its error envelope");
        assert_eq!(err.code, "anchor-not-found");
    }

    /// A forwarded-note envelope (NRN-215): the daemon injects an
    /// `operator_notes` sibling ALONGSIDE the `report` key. `reconstruct` reads
    /// only `report`, so the extra sibling never corrupts the rebuilt
    /// `EditReport` — the note rides on for the routing seam to re-emit on
    /// stderr.
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
        crate::edit::report::render_records(&mut a, &report).unwrap();
        crate::edit::report::render_records(&mut b, &rebuilt).unwrap();
        assert_eq!(a, b, "the notes sibling must not affect the rebuilt report");
    }

    /// A refused envelope missing its `error` is malformed — `reconstruct` errs
    /// so the seam handles it (fall back on dry-run, post-send-uncertain on
    /// apply), rather than panicking in `emit`.
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

    /// `emit` on a refused report exits 2 and writes `error: <message>` prose to
    /// stderr for BOTH formats — unlike `set`, `norn edit`'s direct path does
    /// not special-case `--format json` for a preflight refusal (see `emit`'s
    /// doc comment), so this asserts the exit code holds for both.
    #[test]
    fn emit_refused_exits_two_for_records_and_json() {
        for fmt in [EditFormat::Records, EditFormat::Json] {
            let report = EditReport::refused(
                "notes/note.md".into(),
                ApplyError {
                    code: "target-not-found".to_string(),
                    message: "doc not found: nope".to_string(),
                    path: None,
                },
            );
            let code = emit(report, fmt).unwrap();
            assert_eq!(code, 2, "refusal must exit 2 for {fmt:?}");
        }
    }
}
