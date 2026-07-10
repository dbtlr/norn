//! CLI→service routing translation for `norn delete` (NRN-229 PR B).
//!
//! `delete` wraps the same [`ApplyReport`] on the wire as `move`, rebuilt via
//! [`crate::apply_report::reconstruct_wire_report`]. The `--format json` surface
//! is a straight report projection, so it is routable byte-identically. The
//! `--format records` renderer, however, is only PARTIALLY reconstructable — it
//! reads index-derived incoming-link data (`incoming_total`, the incoming file
//! PATHS, and the RESOLVED `--rewrite-to` target) that never rides the wire
//! `ApplyReport` — so its routability is gated (see `try_route_delete`,
//! `src/lib.rs`):
//!
//! - Routed:
//!   - `--format json` (always — the report is the whole output).
//!   - `--format records` ONLY when neither `--rewrite-to` nor
//!     `--allow-broken-links` is passed. Without either flag, a doc WITH incoming
//!     links is REFUSED at preflight (`backlinks-present`, routed as a coded
//!     refusal), so a SUCCESSFUL records delete under this gate necessarily had
//!     ZERO incoming links — and the renderer then needs no index data (`✓
//!     deleted <doc>` / `norn delete <doc>`).
//! - Gated to Direct:
//!   - `--format records` WITH `--rewrite-to` or `--allow-broken-links` (the
//!     renderer prints incoming counts / file paths / the resolved redirect
//!     target that the wire report omits).
//!   - Any bare-STEM target needing index resolution — `vault.delete` applies the
//!     raw `target` while the CLI arm applies the preflight-RESOLVED path (NRN-57)
//!     — gated by the same on-disk guard `move` uses.

use serde_json::{Map, Value};

use crate::apply_report::{emit_refusal, ApplyOutcome, ApplyReport};
use crate::cli::{DeleteArgs, DeleteFormat};

/// Translate parsed `norn delete` args into the `vault.delete` tool's parameter
/// object (the `DeleteParams` shape in `src/mcp/tools/delete.rs`).
///
/// The wire carries the RAW `target` — the caller has gated any target needing
/// stem resolution. `confirm` is the dry-run/apply switch. `rewrite_to` /
/// `allow_broken_links` are omitted when absent/false (the tool defaults them).
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
///   (json) or the clean `render_delete_apply_tty` (records). The records path is
///   only reached under the neither-flag gate, so incoming links are provably
///   zero and the renderer takes only `doc` + `applied`; the `trace:` footer
///   follows a real apply.
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
            // Routed records deletes are gated to the neither-flag path (no
            // --rewrite-to / --allow-broken-links); a non-refused delete there
            // provably had zero incoming links, so the renderer needs no
            // index-derived data.
            let applied = !dry_run && exit == 0;
            crate::delete_doc::render_delete_apply_tty(
                &mut out,
                doc,
                /*incoming_total=*/ 0,
                /*incoming_files=*/ &[],
                /*rewrite_to=*/ None,
                /*rewrite_total=*/ 0,
                applied,
            )?;
            if !dry_run {
                writeln!(out, "trace: {}", report.trace_id)?;
            }
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
}
