//! CLI→service routing translation for `norn delete` (NRN-229 PR B).
//!
//! `delete` wraps the same [`ApplyReport`] on the wire as `move`, rebuilt via
//! [`crate::apply_report::reconstruct_wire_report`]. Both the `--format json` and
//! `--format records` surfaces are straight report projections: the applier
//! attaches the records renderer's index-derived incoming-link data to the
//! `delete_document` op as `link_impact` (NRN-237), so it rides the wire report
//! and the routed records path reproduces the direct path byte-identically —
//! including the `--rewrite-to` and `--allow-broken-links` shapes.
//!
//! - Routed: `--format json` and `--format records` (all flag combinations),
//!   and every target shape — an exact on-disk `.md` doc path, a bare STEM
//!   needing index resolution, or a stem shadowed by a non-doc on-disk entry
//!   (`foo` beside `foo.md`). Since NRN-239 `vault.delete` runs the SAME
//!   stem-resolving preflight the CLI direct arm does and plans the RESOLVED
//!   target, not the raw argument — raw-vs-resolved divergence is structurally
//!   impossible, so no on-disk existence/extension guard is needed (the
//!   `rewrite_wikilink` model). An unresolvable or ambiguous target still
//!   refuses (coded `target-not-found` / `target-ambiguous`), reconstructed
//!   byte-identically on the wire. `--rewrite-to` needed no such guard even
//!   before NRN-239: BOTH surfaces put the RAW value into the plan fields and
//!   preflight it identically, so it cannot diverge.
//! - Gated to Direct: only the shared flags (`--config` /
//!   `--no-cache-refresh`) and the interactive-TTY path, same as every other
//!   routed mutation.

use serde_json::{Map, Value};

use crate::apply_report::{emit_refusal, ApplyOutcome, ApplyReport};
use crate::cli::{DeleteArgs, DeleteFormat};

/// Translate parsed `norn delete` args into the `vault.delete` tool's parameter
/// object (the `DeleteParams` shape in `src/mcp/tools/delete.rs`).
///
/// The wire carries the RAW `target` — `vault.delete` runs its own
/// stem-resolving preflight and plans the RESOLVED target (NRN-239), so a bare
/// stem here resolves identically to what the direct arm would apply.
/// `confirm` is the dry-run/apply switch. `rewrite_to` / `allow_broken_links`
/// are omitted when absent/false (the tool defaults them).
pub fn to_mcp_arguments(args: &DeleteArgs, confirm: bool) -> Value {
    let mut map = Map::new();
    map.insert("target".into(), Value::String(args.doc.clone()));
    if let Some(alt) = &args.rewrite_to {
        map.insert("rewrite_to".into(), Value::String(alt.clone()));
    }
    if args.allow_broken_links {
        map.insert("allow_broken_links".into(), Value::Bool(true));
    }
    map.insert("confirm".into(), Value::Bool(confirm));
    Value::Object(map)
}

/// Render a reconstructed delete [`ApplyReport`] exactly as the direct `norn
/// delete` arm does, returning the process exit code.
///
/// - **refused**: the pretty `ApplyError` envelope on stdout for json, or `error:
///   <message>` on stderr for records — exit 2.
/// - **applied / dry-run**: cascade-failure warnings to stderr, then the report
///   (json) or the shared `render_delete_records` (records). Since NRN-237 the
///   records renderer's incoming-link inputs (count / files / redirect target)
///   ride the wire report as the delete op's `link_impact`, so this reproduces
///   the direct arm byte-for-byte for the `--rewrite-to` / `--allow-broken-links`
///   shapes too — no longer gated to the zero-incoming-links path.
pub fn emit(
    report: ApplyReport,
    format: DeleteFormat,
    doc: &str,
    dry_run: bool,
) -> anyhow::Result<i32> {
    use std::io::Write as _;

    if matches!(report.outcome, ApplyOutcome::Refused) {
        return emit_refusal(&report, matches!(format, DeleteFormat::Json));
    }

    let exit = report.exit_code();
    crate::emit_cascade_failure_warnings(&report);

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match format {
        DeleteFormat::Json => {
            let json = serde_json::to_string_pretty(&report)?;
            out.write_all(json.as_bytes())?;
            out.write_all(b"\n")?;
        }
        DeleteFormat::Records => {
            crate::delete_doc::render_delete_records(&mut out, &report, doc, dry_run)?;
        }
    }
    Ok(exit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply_report::{ApplyError, ApplyReport};
    use serde_json::json;

    fn base_args() -> DeleteArgs {
        DeleteArgs {
            doc: "doc.md".into(),
            yes: false,
            dry_run: false,
            allow_broken_links: false,
            rewrite_to: None,
            format: DeleteFormat::Records,
        }
    }

    #[test]
    fn to_mcp_arguments_maps_target_confirm_and_omits_defaults() {
        let v = to_mcp_arguments(&base_args(), false);
        assert_eq!(v["target"], "doc.md");
        assert_eq!(v["confirm"], false);
        assert!(v.get("rewrite_to").is_none());
        assert!(v.get("allow_broken_links").is_none());
    }

    #[test]
    fn to_mcp_arguments_maps_rewrite_to_and_allow_broken() {
        let mut args = base_args();
        args.rewrite_to = Some("alt".into());
        let v = to_mcp_arguments(&args, true);
        assert_eq!(v["rewrite_to"], "alt");
        assert_eq!(v["confirm"], true);

        let mut args = base_args();
        args.allow_broken_links = true;
        let v = to_mcp_arguments(&args, true);
        assert_eq!(v["allow_broken_links"], true);
    }

    #[test]
    fn emit_refused_records_exits_two() {
        let report = ApplyReport::refused(
            "/v".into(),
            false,
            "delete_document",
            ApplyError {
                code: "backlinks-present".into(),
                message: "document has 1 incoming link(s); pass --allow-broken-links to accept, or --rewrite-to <ALT_DOC> to redirect".into(),
                path: None,
            },
        );
        let code = emit(report, DeleteFormat::Records, "doc.md", false).unwrap();
        assert_eq!(code, 2);
    }

    #[test]
    fn wire_round_trip_preserves_refused_code() {
        let report = ApplyReport::refused(
            "/v".into(),
            false,
            "delete_document",
            ApplyError {
                code: "backlinks-present".into(),
                message: "boom".into(),
                path: None,
            },
        );
        let wire = json!({ "report": serde_json::to_value(&report).unwrap() });
        let rebuilt = crate::apply_report::reconstruct_wire_report(&wire).unwrap();
        assert_eq!(
            rebuilt.operations[0].error.as_ref().unwrap().code,
            "backlinks-present"
        );
    }

    /// NRN-237: the delete op's index-derived `link_impact` survives the wire
    /// round-trip through `reconstruct_wire_report` unchanged, so the routed
    /// records renderer reads the same incoming-link data the direct arm computed.
    #[test]
    fn wire_round_trip_preserves_link_impact() {
        use crate::apply_report::{
            ApplyOutcome, ApplyReportOp, LinkImpact, OpStatus, APPLY_REPORT_SCHEMA_VERSION,
        };
        let report = ApplyReport {
            schema_version: APPLY_REPORT_SCHEMA_VERSION,
            trace_id: "abc".into(),
            plan_hash: "h".into(),
            vault_root: "/v".into(),
            dry_run: false,
            applied: 1,
            skipped: 0,
            failed: 0,
            remaining: 0,
            operations: vec![ApplyReportOp {
                op_id: "0".into(),
                kind: "delete_document".into(),
                status: OpStatus::Applied,
                from: None,
                path: None,
                stem: None,
                summary: "delete doc.md".into(),
                error: None,
                footnote: None,
                cascade: None,
                link_impact: Some(LinkImpact {
                    incoming_total: 1,
                    incoming_files: vec!["a.md".into()],
                    redirect_to: Some("c.md".into()),
                }),
            }],
            warnings: vec![],
            outcome: ApplyOutcome::Applied,
        };
        let wire = json!({ "report": serde_json::to_value(&report).unwrap() });
        let rebuilt = crate::apply_report::reconstruct_wire_report(&wire).unwrap();
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::to_value(&rebuilt).unwrap(),
            "the full report (incl. link_impact) must round-trip byte-identically"
        );
        let li = rebuilt.operations[0]
            .link_impact
            .as_ref()
            .expect("link_impact must survive the round trip");
        assert_eq!(li.incoming_total, 1);
        assert_eq!(li.incoming_files, vec!["a.md".to_string()]);
        assert_eq!(li.redirect_to.as_deref(), Some("c.md"));
    }
}
