//! CLI→service routing translation for `norn apply` (NRN-231).
//!
//! `apply` ships the PARSED plan over the wire, never a path: the CLI reads the
//! plan source (file or stdin), parses it exactly as the direct arm does
//! ([`crate::apply_cmd::preamble`] — a schema-version mismatch refuses exit 2
//! BEFORE any wire activity, byte-identical to Direct), then re-serializes the
//! [`MigrationPlan`] into the `vault.apply` `plan` argument. YAML input parses
//! client-side and routes the same way — the daemon applies the identical
//! struct, so there is no fidelity loss. The daemon never sees a file path.
//!
//! On the wire `apply` wraps the same [`ApplyReport`] every cascade command does
//! (`vault.move` / `vault.delete` / `vault.rewrite_wikilink`), rebuilt via
//! [`crate::apply_report::reconstruct_wire_report`], and [`emit`] renders it
//! through the SAME [`super::render_report`] the direct tail uses (records / json,
//! `--out` write, the `trace:` footer, cascade-failure warnings, the exit code
//! from `report.exit_code()`), so routed and direct output are byte-for-byte
//! equal.
//!
//! **Lock-timeout stash stays CLI-owned.** The daemon maps its own per-vault
//! mutation-lock timeout to a coded `mutation-lock-timeout` refusal report. When
//! that specific code crosses back, [`emit`] reproduces the direct arm's stash
//! branch byte-identically — stashing a stdin plan under `<state_dir>/pending/`
//! and printing the same `retry with:` hint — rather than rendering the generic
//! refusal (whose daemon-side `CacheError` Display prose differs from the direct
//! arm's hardcoded line). Every OTHER coded refusal renders through the shared
//! `emit_refusal` path.

use serde_json::{Map, Value};

use crate::apply_report::{emit_refusal, ApplyOutcome, ApplyReport};
use crate::cli::ApplyFormat;
use crate::migration_plan::MigrationPlan;
use crate::mutation_lock::pending::save_pending_plan;
use camino::Utf8Path;

/// The daemon-side coded refusal that maps back to the CLI's lock-timeout stash
/// branch instead of the generic refusal renderer.
const MUTATION_LOCK_TIMEOUT_CODE: &str = "mutation-lock-timeout";

/// Translate a parsed `norn apply` plan + mode into the `vault.apply` tool's
/// parameter object (the `ApplyParams` shape in `src/mcp/tools/apply.rs`).
///
/// `plan` crosses as `serde_json::to_value(&plan)` — exactly the value the direct
/// `norn apply --format json` serializes, and exactly what `vault.repair` emits —
/// so a repair→apply composition round-trips byte-identically. `confirm` is the
/// dry-run/apply switch; `parents` is omitted when false (the tool defaults it).
pub fn to_mcp_arguments(plan: &MigrationPlan, confirm: bool, parents: bool) -> Value {
    let mut map = Map::new();
    map.insert(
        "plan".into(),
        serde_json::to_value(plan).unwrap_or(Value::Null),
    );
    map.insert("confirm".into(), Value::Bool(confirm));
    if parents {
        map.insert("parents".into(), Value::Bool(true));
    }
    Value::Object(map)
}

/// Render a reconstructed apply [`ApplyReport`] exactly as the direct `norn
/// apply` tail does, returning the process exit code.
///
/// - **refused with `mutation-lock-timeout`**: reproduce the direct arm's stash
///   branch byte-identically — for a stdin plan (`plan_path == "-"`) stash the
///   RAW bytes under `<state_dir>/pending/` and print the `retry with:` hint;
///   for a file plan print just the error line. Exit 2.
/// - **any other refused**: the shared `emit_refusal` — the pretty `ApplyError`
///   envelope on stdout (json) or `error: <message>` on stderr (records). Exit 2.
/// - **applied / dry-run / failed**: cascade-failure warnings, then the shared
///   [`super::render_report`] (`--out` write, records/json, `trace:` footer), and
///   the pending-plan self-clean on a successful `/pending/` retry.
#[allow(clippy::too_many_arguments)]
pub fn emit(
    report: ApplyReport,
    format: ApplyFormat,
    out: Option<&str>,
    plan_path: &str,
    raw: &str,
    state_dir: &Utf8Path,
) -> anyhow::Result<i32> {
    if matches!(report.outcome, ApplyOutcome::Refused) {
        // The one refusal the CLI re-owns: a daemon-side mutation-lock timeout is
        // reproduced as the direct arm's stash branch, NOT the generic refusal
        // (whose daemon `CacheError` Display prose differs from the hardcoded
        // line the direct arm prints).
        if report
            .operations
            .iter()
            .filter_map(|o| o.error.as_ref())
            .any(|e| e.code == MUTATION_LOCK_TIMEOUT_CODE)
        {
            return emit_lock_timeout_stash(plan_path, raw, state_dir);
        }
        return emit_refusal(&report, matches!(format, ApplyFormat::Json));
    }

    let exit = report.exit_code();
    crate::emit_cascade_failure_warnings(&report);
    super::render_report(&report, format, out)?;

    // Delete the pending plan on a successful retry so it self-cleans — the same
    // shared check the direct tail makes.
    super::self_clean_pending_plan(exit, plan_path);

    Ok(exit)
}

/// Reproduce (and, since NRN-231 review F5, SHARE with) the direct arm's
/// lock-timeout stash branch (`apply_cmd::run_direct`'s `MutationLockTimeout`
/// arm): stash a stdin plan and print the `retry with:` hint, or print just the
/// error for a file plan. Exit 2. The direct arm calls straight into this, so
/// the prose strings and the stdin-vs-file branch exist exactly once.
pub(super) fn emit_lock_timeout_stash(
    plan_path: &str,
    raw: &str,
    state_dir: &Utf8Path,
) -> anyhow::Result<i32> {
    if plan_path == "-" {
        match save_pending_plan(state_dir, raw) {
            Ok(pending_path) => {
                eprintln!("error: another norn mutation is in progress against this vault (timed out after 5 s)");
                eprintln!("retry with: norn apply {pending_path}");
            }
            Err(save_err) => {
                eprintln!("error: another norn mutation is in progress against this vault (timed out after 5 s)");
                eprintln!("warning: could not save stdin plan for retry: {save_err}");
            }
        }
    } else {
        eprintln!(
            "error: another norn mutation is in progress against this vault (timed out after 5 s)"
        );
    }
    Ok(super::EXIT_PREFLIGHT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply_report::{
        ApplyError, ApplyOutcome, ApplyReport, ApplyReportOp, OpStatus, APPLY_REPORT_SCHEMA_VERSION,
    };
    use crate::migration_plan::{MigrationOp, MigrationPlan, MIGRATION_PLAN_SCHEMA_VERSION};
    use serde_json::json;

    fn sample_plan() -> MigrationPlan {
        MigrationPlan {
            schema_version: MIGRATION_PLAN_SCHEMA_VERSION,
            vault_root: "/vault".into(),
            generator: None,
            generated_at: None,
            operations: vec![MigrationOp {
                kind: "create_document".into(),
                id: None,
                requires: vec![],
                fields: json!({"path": "new.md"}),
                footnote: None,
            }],
            skipped: vec![],
            plan_footnote: None,
        }
    }

    #[test]
    fn to_mcp_arguments_ships_plan_value_and_confirm() {
        let plan = sample_plan();
        let v = to_mcp_arguments(&plan, true, false);
        // The plan crosses as the exact `serde_json::to_value(&plan)` value.
        assert_eq!(v["plan"], serde_json::to_value(&plan).unwrap());
        assert_eq!(v["confirm"], true);
        // parents omitted when false (the tool defaults it).
        assert!(v.get("parents").is_none());
    }

    #[test]
    fn to_mcp_arguments_dry_run_and_parents() {
        let plan = sample_plan();
        let v = to_mcp_arguments(&plan, false, true);
        assert_eq!(v["confirm"], false);
        assert_eq!(v["parents"], true);
    }

    /// The plan value round-trips: `to_mcp_arguments`'s `plan` field deserializes
    /// back into an identical `MigrationPlan` (what the daemon does).
    #[test]
    fn plan_value_round_trips_through_wire() {
        let plan = sample_plan();
        let v = to_mcp_arguments(&plan, false, false);
        let rebuilt: MigrationPlan = serde_json::from_value(v["plan"].clone()).unwrap();
        assert_eq!(
            serde_json::to_value(&plan).unwrap(),
            serde_json::to_value(&rebuilt).unwrap(),
        );
    }

    fn refused_report(code: &str) -> ApplyReport {
        ApplyReport::refused(
            "/v".into(),
            false,
            "create_document",
            ApplyError {
                code: code.into(),
                message: format!("{code} boom"),
                path: None,
            },
        )
    }

    /// A non-lock-timeout refusal renders through the shared `emit_refusal`
    /// (records → stderr prose, exit 2), NOT the stash branch.
    #[test]
    fn generic_refusal_exits_two_without_stashing() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let report = refused_report("stale-document-hash");
        let code = emit(report, ApplyFormat::Records, None, "-", "raw", state_dir).unwrap();
        assert_eq!(code, 2);
        // No pending plan was stashed for a non-lock-timeout refusal.
        assert!(
            !state_dir.join("pending").as_std_path().exists(),
            "generic refusal must not stash a pending plan"
        );
    }

    /// A `mutation-lock-timeout` refusal from a STDIN plan stashes the raw bytes
    /// and exits 2 — the CLI-owned stash branch, byte-identical to Direct.
    #[test]
    fn lock_timeout_stdin_stashes_pending_plan() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let report = refused_report(MUTATION_LOCK_TIMEOUT_CODE);
        let raw = "{\"schema_version\":1}";
        let code = emit(report, ApplyFormat::Records, None, "-", raw, state_dir).unwrap();
        assert_eq!(code, 2);
        // Exactly one stashed plan whose contents are the RAW input bytes.
        let pending = state_dir.join("pending");
        let entries: Vec<_> = std::fs::read_dir(pending.as_std_path())
            .expect("pending dir must exist")
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1, "one pending plan stashed");
        assert_eq!(std::fs::read_to_string(&entries[0]).unwrap(), raw);
    }

    /// A `mutation-lock-timeout` refusal from a FILE plan exits 2 and stashes
    /// NOTHING (the direct arm only stashes stdin plans).
    #[test]
    fn lock_timeout_file_plan_does_not_stash() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let report = refused_report(MUTATION_LOCK_TIMEOUT_CODE);
        let code = emit(
            report,
            ApplyFormat::Records,
            None,
            "plan.json",
            "raw",
            state_dir,
        )
        .unwrap();
        assert_eq!(code, 2);
        assert!(
            !state_dir.join("pending").as_std_path().exists(),
            "a file-plan lock-timeout must not stash"
        );
    }

    /// An applied report renders and returns the report's exit code, writing to
    /// `--out` when set (stdout silent).
    #[test]
    fn applied_report_writes_out_file() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let out_path = tmp.path().join("report.json");
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
                kind: "create_document".into(),
                status: OpStatus::Applied,
                from: None,
                path: Some("new.md".into()),
                stem: Some("new".into()),
                summary: "create new.md".into(),
                error: None,
                footnote: None,
                cascade: None,
                link_impact: None,
            }],
            warnings: vec![],
            outcome: ApplyOutcome::Applied,
        };
        let out = out_path.to_str().unwrap();
        let code = emit(
            report,
            ApplyFormat::Json,
            Some(out),
            "plan.json",
            "raw",
            state_dir,
        )
        .unwrap();
        assert_eq!(code, 0);
        let written = std::fs::read_to_string(&out_path).unwrap();
        assert!(written.ends_with('\n'));
        let parsed: Value = serde_json::from_str(written.trim()).unwrap();
        assert_eq!(parsed["applied"], 1);
    }
}
