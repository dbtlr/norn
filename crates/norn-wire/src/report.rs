//! `ApplyReport` — the unified output envelope every mutation returns.
//!
//! One report shape serves apply, move, delete, rewrite-wikilink, and the
//! frontmatter verbs. It is value-out only: the engine builds it and returns it;
//! rendering to a surface (CLI records / JSON / MCP `structuredContent`) is the
//! display layer's job, and the CLI→MCP error-downcast glue and wire
//! reconstruction live in their respective surface crates. This module is the
//! canonical type vocabulary and the two constructors an engine needs at the
//! refusal boundary ([`ApplyReport::refused`]) plus the single outcome→exit
//! mapping ([`ApplyOutcome::exit_code`]).

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

pub const APPLY_REPORT_SCHEMA_VERSION: u32 = 3;

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApplyReport {
    pub schema_version: u32,
    /// Trace ID shared by every telemetry event emitted for this invocation.
    pub trace_id: String,
    /// Set on a CONFIRMED apply whose durable telemetry sink degraded (the
    /// registered vault's events dir/file could not be opened, or a mid-stream
    /// write failed) — never for a dry-run, a refusal, or an unregistered
    /// vault's by-design in-memory sink. Additive (default `false`, omitted
    /// when `false`): when set, `trace_id` above is still non-empty but
    /// correlates to no durable audit line, so a consumer should not treat it
    /// as retrievable via `norn audit` / `vault.audit`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub telemetry_degraded: bool,
    pub plan_hash: String,
    pub vault_root: String,
    pub dry_run: bool,
    pub applied: usize,
    pub skipped: usize,
    pub failed: usize,
    pub remaining: usize,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub preconditions: Vec<ApplyReportPrecondition>,
    pub operations: Vec<ApplyReportOp>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<ApplyWarning>,
    /// The single machine-readable outcome of this apply (NRN-183). Collapses the
    /// exit states — `applied` (exit 0, a confirmed clean write) / `forecast`
    /// (exit 0, a dry-run preview that wrote nothing, NRN-161) / `failed` (exit 1,
    /// a runtime op-failure) / `refused` (exit 2, a validation-phase precondition
    /// refusal) — into ONE field exposed identically by every surface. A consumer
    /// keys on `outcome`, never on inspecting the `failed` count or the process
    /// exit code, to distinguish a refused apply (nothing written) from a
    /// partially-failed one — and (NRN-161) a preview (`forecast`) from a
    /// confirmed write (`applied`) without reading the `dry_run` flag. Defaulted
    /// for backward-compatible deserialization of pre-NRN-183 reports.
    #[serde(default)]
    pub outcome: ApplyOutcome,
    /// Every vault-relative path this apply TOUCHED on disk, populated only on the
    /// clean-commit path. The engine feeds it to the cache-increment commit
    /// (`VaultCacheSlot::commit_apply_increments` in norn-core) so the next
    /// read stays cheap. NOT part of the wire contract — `#[serde(skip)]` keeps it
    /// out of `--format json` / MCP `structuredContent` and defaults it to empty
    /// on deserialize, so a refusal / partial-failure report (which never sets it)
    /// carries no paths.
    #[serde(skip)]
    pub touched_paths: Vec<Utf8PathBuf>,
}

/// The machine-readable outcome of an apply (NRN-183) — the shared vocabulary
/// that reconciles the per-command process-exit codes across surfaces.
///
/// Values are canonically kebab-case per norn's three-form-identity principle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApplyOutcome {
    /// Every op applied (or was a no-op) on a CONFIRMED write — exit 0.
    #[default]
    Applied,
    /// A dry-run preview that writes nothing: the report describes what a
    /// confirmed apply WOULD do, with the same applied/skipped/failed
    /// classification a same-snapshot apply would produce (NRN-161) — exit 0.
    /// Distinct from `Applied` so a consumer keying on `outcome` alone tells a
    /// preview from a real write; the report's `dry_run: true` flag stays
    /// alongside as a direct convenience. A dry-run whose plan WOULD refuse still
    /// reports `Refused` (a forecasted refusal), never `Forecast`.
    Forecast,
    /// At least one op FAILED at runtime after the apply began — exit 1.
    Failed,
    /// A validation-phase precondition refused the plan before any write; the
    /// vault is unchanged — exit 2.
    Refused,
    /// RESERVED for rebase-on-drift (NRN-152): a plan whose stale-hash
    /// precondition was auto-rebased onto current content and re-applied. NOT
    /// produced today; reserved so the outcome vocabulary is forward-compatible
    /// without a breaking add.
    #[allow(dead_code)]
    Rebased,
}

impl ApplyOutcome {
    /// The process exit code this outcome maps to: `applied` → 0, `failed` → 1
    /// (runtime op-failure), `refused` → 2 (preflight refusal). `rebased` is
    /// reserved (NRN-152) and maps to 0 until implemented. This is the one place
    /// the outcome→exit mapping is defined, shared by every surface.
    pub fn exit_code(self) -> i32 {
        match self {
            ApplyOutcome::Applied | ApplyOutcome::Forecast | ApplyOutcome::Rebased => 0,
            ApplyOutcome::Failed => 1,
            ApplyOutcome::Refused => 2,
        }
    }
}

impl ApplyReport {
    /// The process exit code for this report's outcome — delegates to
    /// [`ApplyOutcome::exit_code`], the single outcome→exit mapping.
    pub fn exit_code(&self) -> i32 {
        self.outcome.exit_code()
    }

    /// Build a minimal REFUSED report (`outcome: refused`, exit 2) carrying a
    /// coded [`ApplyError`] envelope, for a mutation whose PREFLIGHT (or mutation-lock
    /// acquisition) refused BEFORE any plan/apply context existed. A refusal
    /// writes nothing, so the vault is unchanged: one `failed` op holds the
    /// `{code, message, path?}` envelope.
    ///
    /// `dry_run` records whether the refused call was a `confirm: false` forecast,
    /// so a surface never flags a preview as a failed tool call.
    pub fn refused(vault_root: String, dry_run: bool, op_kind: &str, error: ApplyError) -> Self {
        let path = error.path.clone();
        Self {
            schema_version: APPLY_REPORT_SCHEMA_VERSION,
            trace_id: String::new(),
            telemetry_degraded: false,
            plan_hash: String::new(),
            vault_root,
            dry_run,
            applied: 0,
            skipped: 0,
            failed: 1,
            remaining: 0,
            preconditions: Vec::new(),
            operations: vec![ApplyReportOp {
                op_id: "0".to_string(),
                kind: op_kind.to_string(),
                status: OpStatus::Failed,
                from: None,
                path,
                stem: None,
                summary: error.message.clone(),
                error: Some(error),
                footnote: None,
                cascade: None,
                link_impact: None,
                finding_code: None,
                repair_rule: None,
            }],
            warnings: Vec::new(),
            outcome: ApplyOutcome::Refused,
            // A refusal writes nothing — no touched paths.
            touched_paths: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApplyReportPrecondition {
    pub id: String,
    pub status: PreconditionStatus,
    pub expected_paths: Vec<String>,
    pub actual_paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<ApplyError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PreconditionStatus {
    Passed,
    Failed,
    NotRun,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApplyReportOp {
    pub op_id: String,
    pub kind: String,
    pub status: OpStatus,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub from: Option<String>,
    /// Structured, apply-time-resolved target path for this op — the value a
    /// consumer would otherwise have to parse out of `summary` prose. Populated
    /// where a single target path is naturally at hand: the `{{seq}}`-resolved
    /// destination of a `create_document`, the destination of a `move_document`,
    /// and the target of a body/section edit. `None` for ops with no single
    /// natural path (e.g. link rewrites, deletes). `summary` stays free prose;
    /// correlate structured data by `op_id`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub path: Option<String>,
    /// Filename stem of [`ApplyReportOp::path`] (its final path component minus
    /// the extension), populated whenever `path` is. Lets a consumer key on the
    /// created/edited document's stem without re-deriving it from `path`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub stem: Option<String>,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<ApplyError>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub footnote: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cascade: Option<CascadeSummary>,
    /// Index-derived, PLANNING-TIME link impact for a `delete_document` op — the
    /// incoming-backlink data the records renderer prints (count, distinct source
    /// files, resolved redirect target). Contrast with [`ApplyReportOp::cascade`],
    /// which reports apply-time ACTUALS: `link_impact` is what the graph index said
    /// BEFORE the delete (identical on the dry-run forecast and the confirmed
    /// apply), computed once in the applier so both the direct and warm-owner
    /// paths render identically without re-consulting the index. Populated
    /// ONLY for `delete_document` ops; `None` for every other op kind, so their
    /// serialized bytes are unchanged.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub link_impact: Option<LinkImpact>,
    /// Finding-provenance linkage echoed verbatim from the op that produced this
    /// record (ADR 0022): the validation finding code a repair-generated op is
    /// resolving. Present only when the op carried it (repair-sourced plans);
    /// `None` — and thus absent from the JSON — for verb-synthesized and authored
    /// ops that declare no linkage, so existing verb-driven report bytes are
    /// unchanged. Carried for provenance only; the applier never reads it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub finding_code: Option<String>,
    /// Finding-provenance linkage echoed verbatim from the op (ADR 0022): the
    /// repair rule the op applies. Present/absent on the same terms as
    /// [`ApplyReportOp::finding_code`]; provenance only, never read for behavior.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub repair_rule: Option<String>,
}

/// Index-derived, planning-time incoming-link impact of a `delete_document` op
/// (NRN-237) — the values the `--format records` renderer needs that the
/// apply-time [`CascadeSummary`] does not carry.
///
/// Computed once in the applier from the graph index, so it rides the wire
/// `ApplyReport` and the routed (warm-owner) records path reproduces the direct
/// path exactly. Distinct from `cascade` (apply-time actuals): this is the
/// pre-delete index view, identical on a dry-run forecast and a confirmed apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkImpact {
    /// Count of incoming backlinks to the deleted document (its `backlinks` len).
    pub incoming_total: usize,
    /// Distinct, sorted, vault-relative source files holding those backlinks. When
    /// `--rewrite-to` is set and no backlink resolves to the deleted doc, this
    /// falls back to the `link_risk` rewrite-source files, matching the direct arm.
    pub incoming_files: Vec<String>,
    /// The resolved `--rewrite-to` target (index-resolved from the raw argument),
    /// or `None` when no redirect was requested. Omitted from the wire when absent.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub redirect_to: Option<String>,
}

/// Per-op summary of the backlink cascade triggered by a `move_document` or
/// `delete_document` op. Counts (`planned`/`applied`/`skipped`/`files`) are
/// always present; `rewrites`/`skips` lists are populated only under
/// `--verbose`.
///
/// - `planned`  — backlinks the plan intended to rewrite (from `link_risk`).
/// - `applied`  — backlinks actually rewritten on disk (the actual, not the forecast).
/// - `skipped`  — planned-not-applied (drift); each carries a reason.
/// - `failed`   — backlinks that hit a real FS error and remained un-rewritten.
/// - `files`    — distinct files actually rewritten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CascadeSummary {
    pub planned: usize,
    pub applied: usize,
    pub skipped: usize,
    /// Backlinks that hit a real FS error and remained un-rewritten after the
    /// retry pass (dangling). Always present.
    pub failed: usize,
    pub files: usize,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub rewrites: Vec<CascadeRewrite>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub skips: Vec<CascadeSkip>,
    /// Per-failure detail. NOT verbose-gated — a failure is ERROR-severity and
    /// must be visible by default (and feeds the stderr warning). Present
    /// whenever non-empty.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub failures: Vec<CascadeFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CascadeFailure {
    pub file: String,
    pub from: String,
    pub to: String,
    /// Reason code: `read-failed` | `write-failed`.
    pub reason: String,
    /// The underlying io error string (e.g. "Permission denied (os error 13)").
    /// Present when known; the actionable "why" behind the reason code.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CascadeRewrite {
    pub file: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CascadeSkip {
    pub file: String,
    pub from: String,
    pub to: String,
    /// Reason code (v1: `"drifted"`). Extensible — a later slice adds failure codes.
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OpStatus {
    Applied,
    Skipped,
    Failed,
    NotRun,
}

/// The structured failure envelope (NRN-150): a stable machine-branchable `code`
/// (kebab), a human `message`, and the offending `path` when one is known.
///
/// Serves two roles with one shape: it is both the per-op `error` on a
/// [`ApplyReportOp`] (report-on-refusal) AND the top-level error envelope a
/// surface consumer branches on. A consumer distinguishes RETRYABLE CAS drift
/// (`stale-document-hash` / `expected-old-value-mismatch`) from a TERMINAL
/// refusal by comparing `code`, never the prose.
///
/// The surface glue that DOWNCASTS a typed engine error into this envelope
/// (`from_rich` / `from_anyhow`) lives in the presentation / wire
/// crates that own those error types, not here — this module owns only the
/// value shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApplyError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApplyWarning {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub path: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NRN-229: the `refused` constructor builds a minimal `outcome: refused`
    /// report (exit 2) with the coded envelope in a single `failed` op, and
    /// records `dry_run` so the `isError` derivation can tell a forecast apart.
    #[test]
    fn refused_builds_a_coded_refusal_report() {
        let env = ApplyError {
            code: "target-not-found".into(),
            message: "doc not found".into(),
            path: None,
        };
        let report = ApplyReport::refused("/v".into(), false, "delete_document", env);
        assert_eq!(report.outcome, ApplyOutcome::Refused);
        assert_eq!(report.exit_code(), 2);
        assert_eq!(report.applied, 0);
        assert_eq!(report.failed, 1);
        assert_eq!(report.operations.len(), 1);
        assert_eq!(report.operations[0].kind, "delete_document");
        assert_eq!(report.operations[0].status, OpStatus::Failed);
        assert_eq!(
            report.operations[0].error.as_ref().unwrap().code,
            "target-not-found"
        );
    }

    #[test]
    fn apply_report_serializes_with_per_op_status() {
        let report = ApplyReport {
            schema_version: APPLY_REPORT_SCHEMA_VERSION,
            trace_id: "".into(),
            telemetry_degraded: false,
            plan_hash: "abc123".into(),
            vault_root: "/abs/vault".into(),
            dry_run: false,
            applied: 1,
            skipped: 0,
            failed: 0,
            remaining: 0,
            preconditions: Vec::new(),
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
                cascade: None,
                link_impact: None,
                finding_code: None,
                repair_rule: None,
            }],
            warnings: vec![],
            outcome: ApplyOutcome::Applied,
            touched_paths: Vec::new(),
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: ApplyReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.applied, 1);
        assert_eq!(back.operations[0].status, OpStatus::Applied);
        assert_eq!(back.outcome, ApplyOutcome::Applied);
    }

    /// NRN-183: `outcome` serializes kebab and round-trips; a pre-NRN-183 report
    /// with no `outcome` field defaults to `applied` on deserialization.
    #[test]
    fn outcome_serializes_kebab_and_defaults() {
        assert_eq!(
            serde_json::to_string(&ApplyOutcome::Refused).unwrap(),
            "\"refused\""
        );
        assert_eq!(
            serde_json::to_string(&ApplyOutcome::Failed).unwrap(),
            "\"failed\""
        );
        // A report JSON emitted before `outcome` existed still deserializes.
        let legacy = serde_json::json!({
            "schema_version": 2,
            "trace_id": "",
            "plan_hash": "h",
            "vault_root": "/v",
            "dry_run": false,
            "applied": 0,
            "skipped": 0,
            "failed": 0,
            "remaining": 0,
            "operations": [],
        });
        let back: ApplyReport = serde_json::from_value(legacy).unwrap();
        assert_eq!(back.outcome, ApplyOutcome::Applied);
        assert_eq!(back.exit_code(), 0);
    }

    /// NRN-150: the error envelope carries `path` only when present.
    #[test]
    fn error_envelope_omits_path_when_absent() {
        let with_path = ApplyError {
            code: "stale-document-hash".into(),
            message: "drifted".into(),
            path: Some("a.md".into()),
        };
        let json = serde_json::to_value(&with_path).unwrap();
        assert_eq!(json["code"], "stale-document-hash");
        assert_eq!(json["path"], "a.md");

        let pathless = ApplyError {
            code: "internal-error".into(),
            message: "boom".into(),
            path: None,
        };
        let pj = serde_json::to_value(&pathless).unwrap();
        assert!(pj.get("path").is_none(), "path omitted when None");
    }

    #[test]
    fn op_status_serializes_as_kebab_case() {
        // NRN-190: op-status VALUES are canonically kebab on the wire, matching
        // the sibling `ApplyOutcome` and the kebab skip-reason codes.
        let json = serde_json::to_string(&OpStatus::NotRun).unwrap();
        assert_eq!(json, "\"not-run\"");
        let parsed: OpStatus = serde_json::from_str("\"failed\"").unwrap();
        assert_eq!(parsed, OpStatus::Failed);
    }

    #[test]
    fn cascade_summary_serializes_counts_always_lists_when_present() {
        let op = ApplyReportOp {
            op_id: "0".into(),
            kind: "move_document".into(),
            status: OpStatus::Applied,
            from: None,
            path: None,
            stem: None,
            summary: "moved a.md → b.md".into(),
            error: None,
            footnote: None,
            cascade: Some(CascadeSummary {
                planned: 3,
                applied: 2,
                skipped: 1,
                failed: 0,
                files: 2,
                rewrites: vec![CascadeRewrite {
                    file: "x.md".into(),
                    from: "[[a]]".into(),
                    to: "[[b]]".into(),
                }],
                skips: vec![CascadeSkip {
                    file: "y.md".into(),
                    from: "[[a]]".into(),
                    to: "[[b]]".into(),
                    reason: "drifted".into(),
                }],
                failures: vec![],
            }),
            link_impact: None,
            finding_code: None,
            repair_rule: None,
        };
        let json = serde_json::to_value(&op).unwrap();
        assert_eq!(json["cascade"]["planned"], 3);
        assert_eq!(json["cascade"]["applied"], 2);
        assert_eq!(json["cascade"]["skipped"], 1);
        assert_eq!(json["cascade"]["files"], 2);
        assert_eq!(json["cascade"]["skips"][0]["reason"], "drifted");

        let bare = ApplyReportOp {
            op_id: "1".into(),
            kind: "set_frontmatter".into(),
            status: OpStatus::Applied,
            from: None,
            path: None,
            stem: None,
            summary: "set type".into(),
            error: None,
            footnote: None,
            cascade: None,
            link_impact: None,
            finding_code: None,
            repair_rule: None,
        };
        let bare_json = serde_json::to_value(&bare).unwrap();
        assert!(bare_json.get("cascade").is_none());
    }

    /// The additive `path` / `stem` fields serialize only when populated, so an
    /// op with no natural resolved path serializes identically to the shape
    /// without those fields.
    #[test]
    fn path_and_stem_skip_serialize_when_none_present_when_some() {
        let bare = ApplyReportOp {
            op_id: "0".into(),
            kind: "rewrite_link".into(),
            status: OpStatus::Applied,
            from: None,
            path: None,
            stem: None,
            summary: "rewrite link".into(),
            error: None,
            footnote: None,
            cascade: None,
            link_impact: None,
            finding_code: None,
            repair_rule: None,
        };
        let bare_json = serde_json::to_value(&bare).unwrap();
        assert!(bare_json.get("path").is_none(), "path omitted when None");
        assert!(bare_json.get("stem").is_none(), "stem omitted when None");

        let populated = ApplyReportOp {
            op_id: "1".into(),
            kind: "create_document".into(),
            status: OpStatus::Applied,
            from: None,
            path: Some("tasks/task-7.md".into()),
            stem: Some("task-7".into()),
            summary: "create tasks/task-7.md".into(),
            error: None,
            footnote: None,
            cascade: None,
            link_impact: None,
            finding_code: None,
            repair_rule: None,
        };
        let pop_json = serde_json::to_value(&populated).unwrap();
        assert_eq!(pop_json["path"], "tasks/task-7.md");
        assert_eq!(pop_json["stem"], "task-7");
    }

    #[test]
    fn cascade_summary_serializes_failed_count_and_failures_list() {
        let summary = CascadeSummary {
            planned: 3,
            applied: 1,
            skipped: 1,
            failed: 1,
            files: 1,
            rewrites: vec![],
            skips: vec![],
            failures: vec![CascadeFailure {
                file: "d.md".into(),
                from: "[[a]]".into(),
                to: "[[b]]".into(),
                reason: "write-failed".into(),
                detail: Some("Permission denied (os error 13)".into()),
            }],
        };
        let json = serde_json::to_value(&summary).unwrap();
        assert_eq!(json["failed"], 1);
        assert_eq!(json["failures"][0]["reason"], "write-failed");
        assert_eq!(json["failures"][0]["file"], "d.md");
        assert_eq!(
            json["failures"][0]["detail"],
            "Permission denied (os error 13)"
        );
    }

    /// `link_impact` serializes only when populated (a `delete_document` op), so
    /// a sibling verb's op serializes identically to the shape without it;
    /// `redirect_to` is omitted when the delete carried no `--rewrite-to`.
    #[test]
    fn link_impact_skips_serialize_when_none_present_when_some() {
        let sibling = ApplyReportOp {
            op_id: "0".into(),
            kind: "move_document".into(),
            status: OpStatus::Applied,
            from: None,
            path: None,
            stem: None,
            summary: "moved a.md → b.md".into(),
            error: None,
            footnote: None,
            cascade: None,
            link_impact: None,
            finding_code: None,
            repair_rule: None,
        };
        let sibling_json = serde_json::to_value(&sibling).unwrap();
        assert!(
            sibling_json.get("link_impact").is_none(),
            "a non-delete op emits no link_impact key"
        );

        let deleted = ApplyReportOp {
            op_id: "1".into(),
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
                incoming_total: 2,
                incoming_files: vec!["x.md".into(), "y.md".into()],
                redirect_to: None,
            }),
            finding_code: None,
            repair_rule: None,
        };
        let del_json = serde_json::to_value(&deleted).unwrap();
        assert_eq!(del_json["link_impact"]["incoming_total"], 2);
        assert_eq!(del_json["link_impact"]["incoming_files"][0], "x.md");
        assert_eq!(del_json["link_impact"]["incoming_files"][1], "y.md");
        assert!(
            del_json["link_impact"].get("redirect_to").is_none(),
            "redirect_to omitted when no --rewrite-to"
        );

        let redirected = LinkImpact {
            incoming_total: 1,
            incoming_files: vec!["x.md".into()],
            redirect_to: Some("alt.md".into()),
        };
        let rj = serde_json::to_value(&redirected).unwrap();
        assert_eq!(rj["redirect_to"], "alt.md");
    }

    /// NRN-237: the field is additive + serde-defaulted, so a pre-NRN-237 report
    /// JSON (no `link_impact` key on any op) still deserializes.
    #[test]
    fn pre_nrn237_report_without_link_impact_still_deserializes() {
        let legacy = serde_json::json!({
            "schema_version": 2,
            "trace_id": "",
            "plan_hash": "h",
            "vault_root": "/v",
            "dry_run": false,
            "applied": 1,
            "skipped": 0,
            "failed": 0,
            "remaining": 0,
            "operations": [{
                "op_id": "0",
                "kind": "delete_document",
                "status": "applied",
                "summary": "delete doc.md",
            }],
        });
        let back: ApplyReport = serde_json::from_value(legacy).unwrap();
        assert_eq!(back.operations.len(), 1);
        assert!(back.operations[0].link_impact.is_none());
    }
}
