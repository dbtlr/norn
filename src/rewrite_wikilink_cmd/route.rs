//! CLI→service routing translation for `norn rewrite-wikilink` (NRN-229 PR B).
//!
//! `rewrite-wikilink` wraps the same [`ApplyReport`] on the wire as `move` /
//! `delete`, rebuilt via [`crate::apply_report::reconstruct_wire_report`]. It is
//! the CLEANEST cascade command to route: unlike `move`/`delete`, BOTH the CLI
//! arm (`rewrite_wikilink_cmd::run`) and `vault.rewrite_wikilink` build the plan
//! from the RAW `{old, new}` (the expander resolves OLD internally, identically
//! on both paths), so there is no stem-resolution divergence and NO on-disk gate
//! is needed — every input routes, including an unresolvable OLD (a
//! `target-not-found` coded refusal).
//!
//! - Routed: `old`, `new`, `--format`, `--out`.
//! - `--out <path>` is reproduced CLIENT-SIDE in [`emit`] (write the pretty JSON
//!   report to the file, silence stdout) — it is not a gating reason.
//! - Gated to Direct: only the shared forced-Direct flags (`--config` /
//!   `--no-cache-refresh`) and the interactive-TTY apply path.

use serde_json::{Map, Value};

use crate::apply_report::{emit_refusal, ApplyOutcome, ApplyReport};
use crate::cli::RewriteWikilinkFormat;
use crate::rewrite_wikilink_cmd::RewriteWikilinkRunArgs;

/// Translate the `norn rewrite-wikilink` run args into the
/// `vault.rewrite_wikilink` tool's parameter object (`RewriteWikilinkParams`).
///
/// The wire carries the RAW `from`/`to` (== the CLI's raw `old`/`new`).
/// `confirm` is the dry-run/apply switch.
pub fn to_mcp_arguments(args: &RewriteWikilinkRunArgs, confirm: bool) -> Value {
    let mut map = Map::new();
    map.insert("from".into(), Value::String(args.old.clone()));
    map.insert("to".into(), Value::String(args.new.clone()));
    map.insert("confirm".into(), Value::Bool(confirm));
    Value::Object(map)
}

/// Render a reconstructed rewrite-wikilink [`ApplyReport`] exactly as
/// `rewrite_wikilink_cmd::render_report` does, returning the process exit code.
///
/// - **refused** (`target-not-found`, OLD unresolvable): the pretty `ApplyError`
///   envelope on stdout for json, or `error: <message>` on stderr for records —
///   exit 2, BEFORE any `--out` handling (the direct arm refuses before render, so
///   `--out` is never written on a refusal).
/// - **applied / dry-run**: with `--out`, the pretty JSON report is written to the
///   file and stdout stays silent; otherwise json (report) or records (body /
///   frontmatter op-count breakdown, `trace:` footer on a real apply).
pub fn emit(report: ApplyReport, args: &RewriteWikilinkRunArgs) -> anyhow::Result<i32> {
    use anyhow::Context as _;
    use std::io::Write as _;

    if matches!(report.outcome, ApplyOutcome::Refused) {
        return emit_refusal(&report, matches!(args.format, RewriteWikilinkFormat::Json));
    }

    let exit = report.exit_code();

    // `--out`: write the pretty JSON report to the file, silence stdout — the
    // same mutually-exclusive contract `render_report` enforces.
    if let Some(out_path) = &args.out {
        let json = serde_json::to_string_pretty(&report)?;
        std::fs::write(out_path, format!("{json}\n"))
            .with_context(|| format!("failed to write apply report to '{out_path}'"))?;
        return Ok(exit);
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match args.format {
        RewriteWikilinkFormat::Json => {
            let json = serde_json::to_string_pretty(&report)?;
            out.write_all(json.as_bytes())?;
            out.write_all(b"\n")?;
        }
        RewriteWikilinkFormat::Records => {
            // `render_records` is module-private; a submodule may reach it.
            crate::rewrite_wikilink_cmd::render_records(&report, &args.old, &args.new, &mut out)?;
            if !report.dry_run {
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

    fn base_args() -> RewriteWikilinkRunArgs {
        RewriteWikilinkRunArgs {
            old: "old-target".into(),
            new: "new-target".into(),
            dry_run: false,
            yes: false,
            format: RewriteWikilinkFormat::Records,
            out: None,
        }
    }

    #[test]
    fn to_mcp_arguments_maps_from_to_confirm() {
        let v = to_mcp_arguments(&base_args(), true);
        assert_eq!(v["from"], "old-target");
        assert_eq!(v["to"], "new-target");
        assert_eq!(v["confirm"], true);
    }

    #[test]
    fn emit_refused_json_exits_two() {
        let report = ApplyReport::refused(
            "/v".into(),
            false,
            "rewrite_wikilink",
            ApplyError {
                code: "target-not-found".into(),
                message: "no document resolves to wikilink target 'old-target'".into(),
                path: None,
            },
        );
        let mut args = base_args();
        args.format = RewriteWikilinkFormat::Json;
        let code = emit(report, &args).unwrap();
        assert_eq!(code, 2);
    }

    #[test]
    fn emit_out_writes_file_and_silences_stdout() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-rw-route-")
            .tempdir()
            .unwrap();
        let out_path = tmp.path().join("report.json");
        let report = ApplyReport {
            schema_version: crate::apply_report::APPLY_REPORT_SCHEMA_VERSION,
            trace_id: "abc".into(),
            plan_hash: "h".into(),
            vault_root: "/v".into(),
            dry_run: false,
            applied: 1,
            skipped: 0,
            failed: 0,
            remaining: 0,
            operations: vec![],
            warnings: vec![],
            outcome: ApplyOutcome::Applied,
        };
        let mut args = base_args();
        args.out = Some(out_path.to_string_lossy().into_owned());
        let code = emit(report, &args).unwrap();
        assert_eq!(code, 0);
        let written = std::fs::read_to_string(&out_path).unwrap();
        assert!(written.ends_with("\n"), "report file ends with a newline");
        let parsed: Value = serde_json::from_str(written.trim()).unwrap();
        assert_eq!(parsed["applied"], 1);
    }

    #[test]
    fn wire_round_trip_preserves_report() {
        let report = ApplyReport {
            schema_version: crate::apply_report::APPLY_REPORT_SCHEMA_VERSION,
            trace_id: String::new(),
            plan_hash: "h".into(),
            vault_root: "/v".into(),
            dry_run: true,
            applied: 0,
            skipped: 0,
            failed: 0,
            remaining: 0,
            operations: vec![],
            warnings: vec![],
            outcome: ApplyOutcome::Applied,
        };
        let wire = json!({ "report": serde_json::to_value(&report).unwrap() });
        let rebuilt = crate::apply_report::reconstruct_wire_report(&wire).unwrap();
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::to_value(&rebuilt).unwrap()
        );
    }
}
