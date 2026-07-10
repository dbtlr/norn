//! CLI→service routing translation for `norn move` (NRN-229 PR B).
//!
//! `move` is the third routed mutation, following the `set`/`edit` template
//! (`src/set/route.rs`). It is routable byte-identically because the
//! `vault.move` MCP tool returns an [`ApplyReport`]-shaped `structuredContent`
//! (`{ "report": <ApplyReport>, ... }`) that
//! [`crate::apply_report::reconstruct_wire_report`] rebuilds into the native
//! [`ApplyReport`], which this module renders through the SAME
//! `move_doc::{render_move_apply_tty, render_folder_apply_tty}` and the SAME
//! JSON projection the direct arm uses — so a routed `norn move` and a direct
//! one are byte-for-byte equal on stdout, stderr, and exit code (the load-bearing
//! isomorphism, ADR 0005).
//!
//! **Routable surface** (the gating lives in `try_route_move`, `src/lib.rs`):
//!
//! - Routed: a move whose SOURCE argument is a real path on disk (a single `.md`
//!   file, or a directory → folder move). Both the CLI arm and `vault.move` apply
//!   the raw destination and the (raw == on-disk) source, so the plans agree.
//! - Gated to Direct: a bare-STEM source that requires index resolution. The CLI
//!   arm applies the preflight-RESOLVED path (NRN-216) while `vault.move` applies
//!   the raw argument, so a resolvable stem would diverge (direct moves the
//!   resolved doc; routed no-ops/fails on the literal stem). A missing source is
//!   caught by the same on-disk guard and refuses identically on the Direct path.
//!
//! `remove_empty_dirs` after a live folder move runs on BOTH surfaces — the CLI
//! arm (`src/lib.rs`) AND inside `vault.move`'s handler
//! (`src/mcp/tools/move_doc.rs`) — so the daemon has already cleaned the empty
//! source tree before returning; the routed client reproduces nothing and the
//! post-state vault stays byte-identical.

use serde_json::{Map, Value};

use crate::apply_report::{emit_refusal, ApplyOutcome, ApplyReport};
use crate::cli::{MoveArgs, MoveFormat};

/// Translate parsed `norn move` args into the `vault.move` tool's parameter
/// object (the `MoveParams` shape in `src/mcp/tools/move_doc.rs`).
///
/// The wire carries the RAW `from`/`to` — the caller (`try_route_move`) has
/// already gated any source needing stem resolution, so raw == the resolved
/// path the direct arm applies. `confirm` is the dry-run/apply switch (false =
/// dry-run/preview, true = apply). Boolean flags are omitted when false (the
/// tool defaults them), matching `set`'s omit-when-empty projection.
pub fn to_mcp_arguments(args: &MoveArgs, confirm: bool) -> Value {
    let mut map = Map::new();
    map.insert("from".into(), Value::String(args.src.clone()));
    map.insert("to".into(), Value::String(args.dst.clone()));
    if args.recursive {
        map.insert("recursive".into(), Value::Bool(true));
    }
    if args.parents {
        map.insert("parents".into(), Value::Bool(true));
    }
    if args.force {
        map.insert("force".into(), Value::Bool(true));
    }
    if args.no_link_rewrite {
        map.insert("no_link_rewrite".into(), Value::Bool(true));
    }
    // `confirm` drives the MCP dry-run/apply contract; always sent explicit.
    map.insert("confirm".into(), Value::Bool(confirm));
    Value::Object(map)
}

/// Render a reconstructed move [`ApplyReport`] exactly as the direct `norn move`
/// arm does, returning the process exit code.
///
/// - **refused** (a coded preflight refusal, NRN-229): reproduce the direct
///   output — the pretty `ApplyError` envelope on stdout for `--format json`, or
///   `error: <message>` prose on stderr for records — and exit 2.
/// - **applied / partial / dry-run**: emit the cascade-failure warnings (stderr,
///   from the report's own cascade — reconstructable), then render the report
///   (single-file `render_move_apply_tty` or `render_folder_apply_tty`), then the
///   records-only `trace:` footer on a real apply. Exit is the report's own
///   `outcome` mapping (0 applied, 1 partial-failure).
pub fn emit(
    report: ApplyReport,
    format: MoveFormat,
    src: &str,
    dst: &str,
    is_folder: bool,
    dry_run: bool,
) -> anyhow::Result<i32> {
    use std::io::Write as _;

    if matches!(report.outcome, ApplyOutcome::Refused) {
        return emit_refusal(&report, matches!(format, MoveFormat::Json));
    }

    let exit = report.exit_code();

    // Cascade-failure warnings go to stderr from the report's own cascade data
    // (`failed` / `failures`), which is on the wire — byte-identical to the
    // direct arm's `emit_cascade_failure_warnings`.
    crate::emit_cascade_failure_warnings(&report);

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match format {
        MoveFormat::Json => {
            let json = serde_json::to_string_pretty(&report)?;
            out.write_all(json.as_bytes())?;
            out.write_all(b"\n")?;
        }
        MoveFormat::Records => {
            if is_folder {
                crate::move_doc::render_folder_apply_tty(&mut out, &report, dry_run)?;
            } else {
                // Single-file cascade counts come from the move_document op's
                // cascade (dry-run: forecast; live: actuals) — the same source
                // the direct arm reads.
                let (link_total, link_files) = report
                    .operations
                    .iter()
                    .find(|o| o.kind == "move_document")
                    .and_then(|o| o.cascade.as_ref())
                    .map_or((0, 0), |c| (c.applied, c.files));
                let applied = !dry_run && exit == 0;
                crate::move_doc::render_move_apply_tty(
                    &mut out, src, dst, link_total, link_files, applied,
                )?;
            }
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
    use crate::apply_report::{
        ApplyReport, ApplyReportOp, CascadeSummary, OpStatus, APPLY_REPORT_SCHEMA_VERSION,
    };
    use serde_json::json;

    fn base_args() -> MoveArgs {
        MoveArgs {
            src: "a.md".into(),
            dst: "b.md".into(),
            yes: false,
            dry_run: false,
            no_link_rewrite: false,
            force: false,
            parents: false,
            recursive: false,
            format: MoveFormat::Records,
        }
    }

    #[test]
    fn to_mcp_arguments_maps_from_to_confirm_and_omits_false_flags() {
        let v = to_mcp_arguments(&base_args(), false);
        assert_eq!(v["from"], "a.md");
        assert_eq!(v["to"], "b.md");
        assert_eq!(v["confirm"], false);
        assert!(v.get("recursive").is_none(), "false recursive omitted");
        assert!(v.get("parents").is_none(), "false parents omitted");
        assert!(v.get("force").is_none(), "false force omitted");
        assert!(
            v.get("no_link_rewrite").is_none(),
            "false no_link_rewrite omitted"
        );
    }

    #[test]
    fn to_mcp_arguments_maps_set_flags_and_confirm() {
        let mut args = base_args();
        args.recursive = true;
        args.parents = true;
        args.force = true;
        args.no_link_rewrite = true;
        let v = to_mcp_arguments(&args, true);
        assert_eq!(v["recursive"], true);
        assert_eq!(v["parents"], true);
        assert_eq!(v["force"], true);
        assert_eq!(v["no_link_rewrite"], true);
        assert_eq!(v["confirm"], true);
    }

    fn move_report(applied: bool, cascade: Option<CascadeSummary>) -> ApplyReport {
        ApplyReport {
            schema_version: APPLY_REPORT_SCHEMA_VERSION,
            trace_id: if applied { "abc".into() } else { String::new() },
            plan_hash: "h".into(),
            vault_root: "/v".into(),
            dry_run: !applied,
            applied: usize::from(applied),
            skipped: 0,
            failed: 0,
            remaining: 0,
            operations: vec![ApplyReportOp {
                op_id: "0".into(),
                kind: "move_document".into(),
                status: OpStatus::Applied,
                from: None,
                path: None,
                stem: None,
                summary: "moved a.md → b.md".into(),
                error: None,
                footnote: None,
                cascade,
            }],
            warnings: vec![],
            outcome: ApplyOutcome::Applied,
        }
    }

    /// The wire report round-trips through `reconstruct_wire_report` unchanged, so
    /// rendering the rebuilt value equals rendering the original.
    #[test]
    fn wire_round_trip_preserves_report() {
        let report = move_report(true, None);
        let wire = json!({ "report": serde_json::to_value(&report).unwrap() });
        let rebuilt = crate::apply_report::reconstruct_wire_report(&wire).unwrap();
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::to_value(&rebuilt).unwrap()
        );
    }

    #[test]
    fn emit_refused_json_exits_two() {
        let report = ApplyReport::refused(
            "/v".into(),
            false,
            "move_document",
            crate::apply_report::ApplyError {
                code: "destination-exists".into(),
                message: "destination already exists: b.md (pass --force to overwrite)".into(),
                path: None,
            },
        );
        let code = emit(report, MoveFormat::Json, "a.md", "b.md", false, false).unwrap();
        assert_eq!(code, 2);
    }

    #[test]
    fn emit_applied_exits_zero() {
        let report = move_report(true, None);
        let code = emit(report, MoveFormat::Records, "a.md", "b.md", false, false).unwrap();
        assert_eq!(code, 0);
    }
}
