//! ApplyReport — the unified output envelope for apply, move, delete,
//! rewrite-wikilink, and future new/set conversions.
//!
//! Replaces MoveReport, DeleteReport, RepairApplyReport.

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

pub const APPLY_REPORT_SCHEMA_VERSION: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyReport {
    pub schema_version: u32,
    /// Trace ID shared by every telemetry event emitted for this invocation.
    pub trace_id: String,
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
    /// process-exit tri-state — `applied` (exit 0) / `failed` (exit 1, a runtime
    /// op-failure) / `refused` (exit 2, a validation-phase precondition refusal) —
    /// into ONE field exposed identically by the CLI (`--format json`) and the MCP
    /// `structuredContent`. A consumer keys on `outcome`, never on inspecting the
    /// `failed` count or the process exit code, to distinguish a refused apply
    /// (nothing written) from a partially-failed one. Defaulted for
    /// backward-compatible deserialization of pre-NRN-183 reports.
    #[serde(default)]
    pub outcome: ApplyOutcome,
    /// Every vault-relative path this apply TOUCHED on disk, populated only on the
    /// clean-commit path (NRN-252 / NRN-158) from `RepairApplyReport::touched_paths`.
    /// The warm MCP mutation tools feed it to the cache-increment commit so the
    /// next read stays cheap; the CLI ignores it. NOT part of the wire contract —
    /// `#[serde(skip)]` keeps it out of `--format json` / MCP `structuredContent`
    /// and defaults it to empty on deserialize, so a refusal / partial-failure
    /// report (which never sets it) carries no paths.
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
    /// Every op applied (or was a no-op), or a dry-run forecast — exit 0.
    #[default]
    Applied,
    /// At least one op FAILED at runtime after the apply began — exit 1.
    Failed,
    /// A validation-phase precondition refused the plan before any write; the
    /// vault is byte-identical — exit 2.
    Refused,
    /// RESERVED for NRN-152 (rebase-on-drift): a plan whose stale-hash
    /// precondition was auto-rebased onto current content and re-applied. NOT
    /// produced today — rebase-on-drift is deferred to NRN-152; reserved here so
    /// the outcome vocabulary is forward-compatible without a breaking add.
    #[allow(dead_code)]
    Rebased,
}

impl ApplyOutcome {
    /// The process exit code this outcome maps to: `applied` → 0, `failed` → 1
    /// (runtime op-failure), `refused` → 2 (preflight refusal). `rebased` is
    /// reserved (NRN-152) and maps to 0 until implemented. This is the one place
    /// the outcome→exit mapping is defined, shared by every CLI mutation arm and
    /// the MCP `isError` derivation ([`MutationResult`](crate::mcp::mutation_result::MutationResult)).
    pub fn exit_code(self) -> i32 {
        match self {
            ApplyOutcome::Applied | ApplyOutcome::Rebased => 0,
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
    /// coded [`ApplyError`] envelope, for an MCP mutation tool whose PREFLIGHT (or
    /// mutation-lock acquisition) refused BEFORE any plan/apply context existed —
    /// `vault.move` / `vault.delete` / `vault.rewrite_wikilink` / `vault.apply`
    /// (NRN-229). A refusal writes nothing, so the vault is byte-identical: one
    /// `failed` op holds the `{code, message, path?}` envelope. This is the
    /// `ApplyReport` counterpart to `SetReport::refused` / `EditReport::refused`.
    ///
    /// `dry_run` records whether the refused call was a `confirm: false` forecast,
    /// so [`MutationResult::from_apply_report`](crate::mcp::mutation_result::MutationResult::from_apply_report)
    /// never flags a preview as a failed tool call.
    pub fn refused(vault_root: String, dry_run: bool, op_kind: &str, error: ApplyError) -> Self {
        let path = error.path.clone();
        Self {
            schema_version: APPLY_REPORT_SCHEMA_VERSION,
            trace_id: String::new(),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// apply), computed once in the applier so both the direct and warm-daemon
    /// (wire) paths render byte-identically without re-consulting the index
    /// (NRN-237). Populated ONLY for `delete_document` ops; `None` for every other
    /// op kind, so their serialized bytes are unchanged. Additive + serde-defaulted:
    /// it did not require a schema bump at introduction (a pre-NRN-237 report
    /// with no `link_impact` key still deserializes, and an op without it
    /// serializes no key).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub link_impact: Option<LinkImpact>,
}

/// Index-derived, planning-time incoming-link impact of a `delete_document` op
/// (NRN-237) — the values the `--format records` renderer needs that the
/// apply-time [`CascadeSummary`] does not carry.
///
/// Computed once in the applier from the graph index (the same `backlinks` /
/// `resolve_target_path` the CLI preflight uses), so it rides the wire
/// `ApplyReport` and the routed (warm-daemon) records path reproduces the direct
/// path byte-for-byte. Distinct from `cascade` (apply-time actuals): this is the
/// pre-delete index view, identical on a dry-run forecast and a confirmed apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkImpact {
    /// Count of incoming backlinks to the deleted document (its `backlinks` len).
    pub incoming_total: usize,
    /// Distinct, sorted, vault-relative source files holding those backlinks. When
    /// `--rewrite-to` is set and no backlink resolves to the deleted doc, this
    /// falls back to the `link_risk` rewrite-source files, matching the direct arm.
    pub incoming_files: Vec<String>,
    /// The resolved `--rewrite-to` target (index-resolved from the raw argument,
    /// the same resolution the CLI preflight performs), or `None` when no redirect
    /// was requested. Omitted from the wire when absent.
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
/// [`ApplyReportOp`] (report-on-refusal) AND the top-level error envelope a CLI
/// `--format json` consumer / an MCP structured-error consumer branches on. A
/// consumer distinguishes RETRYABLE CAS drift (`stale-document-hash` /
/// `expected-old-value-mismatch`) from a TERMINAL refusal by comparing `code`,
/// never the prose.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApplyError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub path: Option<String>,
}

impl ApplyError {
    /// Build the envelope from the rich apply-time error (NRN-150).
    pub fn from_rich(e: &crate::standards::apply::ApplyError) -> Self {
        Self {
            code: e.code().to_string(),
            message: e.to_string(),
            path: e.path().map(|p| p.to_string()),
        }
    }

    /// Build the envelope from a containment error (path escaped the vault root).
    /// Shared by [`from_anyhow`](Self::from_anyhow) and the MCP single-op refusal
    /// seam (`mcp::mutate::refusal_from_error`) so both surface the identical
    /// `{code, message, path}` for a containment refusal.
    pub fn from_containment(e: &crate::standards::apply::ContainmentError) -> Self {
        Self {
            code: e.code().to_string(),
            message: e.to_string(),
            path: Some(e.target().to_string()),
        }
    }

    /// Build the envelope from an opaque `anyhow::Error`, recovering structure by
    /// downcasting through the known failure types. Falls back to a generic
    /// `internal-error` code for anything unrecognized so a JSON consumer ALWAYS
    /// gets `{ code, message }`, never a bare exit + prose. This is the single
    /// seam that turns the CLI/MCP `Err` path into a structured envelope.
    pub fn from_anyhow(e: &anyhow::Error) -> Self {
        if let Some(rich) = e.downcast_ref::<crate::standards::apply::ApplyError>() {
            return Self::from_rich(rich);
        }
        if let Some(c) = e.downcast_ref::<crate::standards::apply::ContainmentError>() {
            return Self::from_containment(c);
        }
        // `norn set`'s schema/argument-refusal family (NRN-221): the Set
        // dispatch arm's `--format json` error path (`render_json_error_envelope`
        // in lib.rs) funnels through here, so a CLI JSON consumer gets the same
        // stable code an MCP `vault.set` client sees via
        // `mcp::mutate::refusal_from_error`. Previously the Set arm emitted only
        // prose on stderr (no envelope at all); the Records/TTY path still does.
        if let Some(se) = e.downcast_ref::<crate::set::error::SetError>() {
            return Self {
                code: se.code().to_string(),
                message: se.to_string(),
                path: None,
            };
        }
        // `norn move` / `norn delete` / `norn rewrite-wikilink`'s typed preflight
        // refusals (NRN-229): the `--format json` error path funnels through here,
        // so a CLI JSON consumer gets the same stable code an MCP client sees via
        // `mcp::mutate::refusal_from_error`. Previously these `anyhow::bail!`-ed
        // into a bare string laundered to `internal-error`.
        if let Some(mv) = e.downcast_ref::<crate::move_doc::MovePreflightError>() {
            return Self {
                code: mv.code().to_string(),
                message: mv.to_string(),
                path: None,
            };
        }
        if let Some(del) = e.downcast_ref::<crate::delete_doc::DeletePreflightError>() {
            return Self {
                code: del.code().to_string(),
                message: del.to_string(),
                path: None,
            };
        }
        if let Some(rw) =
            e.downcast_ref::<crate::planner::intent::rewrite_wikilink::RewriteWikilinkError>()
        {
            return Self {
                code: rw.code().to_string(),
                message: rw.to_string(),
                path: None,
            };
        }
        if let Some(cache) = e.downcast_ref::<crate::cache::CacheError>() {
            if matches!(cache, crate::cache::CacheError::MutationLockTimeout) {
                return Self {
                    code: "mutation-lock-timeout".to_string(),
                    message: cache.to_string(),
                    path: None,
                };
            }
        }
        // A committing routed mutation whose daemon call failed after the
        // request was sent (NRN-228): the vault state is UNKNOWN, so a consumer
        // must branch to inspect/re-read, never blind-retry. No path — the
        // uncertainty spans whatever the tool call targeted.
        if let Some(uncertain) = e.downcast_ref::<crate::service::PostSendUncertainError>() {
            return Self {
                code: uncertain.code().to_string(),
                message: uncertain.to_string(),
                path: None,
            };
        }
        Self {
            code: "internal-error".to_string(),
            // `{:#}` renders the full anyhow context chain, matching the prose the
            // CLI previously printed to stderr.
            message: format!("{e:#}"),
            path: None,
        }
    }
}

/// Rebuild an [`ApplyReport`] from a `vault.move` / `vault.delete` /
/// `vault.rewrite_wikilink` `structuredContent` object (NRN-229 PR B).
///
/// Each tool wraps its report under a `report` key (`MoveOutput` /
/// `DeleteOutput` / `RewriteWikilinkOutput`), so this pulls `structured["report"]`
/// and deserializes it back into the native [`ApplyReport`] — the exact inverse
/// of the daemon's `serde_json::to_value(report)` projection, so rendering the
/// rebuilt value equals rendering the direct value. A refused report MUST carry
/// a coded `error` on either a failed operation or failed precondition (what
/// [`emit_refusal`] renders); a missing one is a malformed envelope, returned as
/// `Err` so the routing seam handles it
/// (fall back to Direct on a dry-run, post-send-uncertain on an apply). Any shape
/// mismatch is likewise an `Err`. The shared analogue of `set::route::reconstruct`
/// / `edit::route::reconstruct` — every cascade command wraps the same
/// `ApplyReport` on the wire.
pub fn reconstruct_wire_report(structured: &serde_json::Value) -> anyhow::Result<ApplyReport> {
    let report_val = structured.get("report").ok_or_else(|| {
        anyhow::anyhow!("mutation envelope: missing `report` object in structuredContent")
    })?;
    let report: ApplyReport = serde_json::from_value(report_val.clone())
        .map_err(|e| anyhow::anyhow!("mutation envelope: unreadable report: {e}"))?;
    if matches!(report.outcome, ApplyOutcome::Refused)
        && !report.operations.iter().any(|o| o.error.is_some())
        && !report.preconditions.iter().any(|p| p.error.is_some())
    {
        anyhow::bail!("mutation envelope: refused report carries no coded error");
    }
    Ok(report)
}

/// Reproduce a direct mutation-command PREFLIGHT-REFUSAL from a reconstructed
/// `outcome: refused` [`ApplyReport`] (the routed path), byte-for-byte and
/// exiting 2 (NRN-229 PR B).
///
/// - `json = true`: the pretty `ApplyError` envelope on stdout, matching
///   `render_json_error_envelope` (which is `to_string_pretty` of the SAME
///   `ApplyError` the daemon's `refusal_from_error` built for the SAME underlying
///   error — identical `{code, message, path?}`).
/// - `json = false`: `error: <message>` prose on stderr, matching the direct
///   arms' `eprintln!("error: {e}")` (`move`/`delete`) and
///   `eprintln!("error: {e:#}")` (`rewrite-wikilink`) — for a single typed
///   preflight error with no anyhow context chain, `{e}` and `{e:#}` render the
///   identical `Display` string the envelope's `message` carries.
pub fn emit_refusal(report: &ApplyReport, json: bool) -> anyhow::Result<i32> {
    use std::io::Write as _;
    let error = report
        .operations
        .iter()
        .find_map(|o| o.error.as_ref())
        .or_else(|| report.preconditions.iter().find_map(|p| p.error.as_ref()))
        .expect("reconstruct_wire_report guarantees a refused report carries a coded error");
    if json {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        let s = serde_json::to_string_pretty(error)?;
        out.write_all(s.as_bytes())?;
        out.write_all(b"\n")?;
    } else {
        eprintln!("error: {}", error.message);
    }
    Ok(2)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyWarning {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub path: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NRN-228: the routed-mutation uncertainty error keeps its stable code
    /// through the anyhow seam — a JSON consumer sees `post-send-uncertain`,
    /// not a laundered `internal-error`.
    #[test]
    fn from_anyhow_recovers_the_post_send_uncertain_code() {
        let error = anyhow::Error::new(crate::service::PostSendUncertainError {
            tool: "vault.set".to_string(),
            cause: anyhow::anyhow!("connection reset"),
        });
        let envelope = ApplyError::from_anyhow(&error);
        assert_eq!(envelope.code, "post-send-uncertain");
        assert!(
            envelope
                .message
                .contains("the daemon may have applied the change"),
            "the message keeps the uncertainty prose; got: {}",
            envelope.message
        );
        assert_eq!(
            envelope.path, None,
            "the uncertainty carries no single path"
        );
    }

    /// NRN-221: `set`'s schema/argument-refusal family keeps its stable code
    /// through the anyhow seam too — a CLI `--format json` consumer sees e.g.
    /// `required-field-removed`, not a laundered `internal-error`.
    #[test]
    fn from_anyhow_recovers_the_set_error_code() {
        let error: anyhow::Error = crate::set::error::SetError::RequiredFieldRemoved {
            field: "status".to_string(),
        }
        .into();
        let envelope = ApplyError::from_anyhow(&error);
        assert_eq!(envelope.code, "required-field-removed");
        assert_eq!(
            envelope.message,
            "cannot remove required field 'status'; use --force to override"
        );
        assert_eq!(envelope.path, None);
    }

    /// NRN-229: the `move` / `delete` / `rewrite-wikilink` typed preflight
    /// refusals keep their stable codes through the anyhow seam — a CLI
    /// `--format json` consumer sees the semantic code, not `internal-error`.
    #[test]
    fn from_anyhow_recovers_the_preflight_refusal_codes() {
        let e: anyhow::Error =
            crate::move_doc::MovePreflightError::DestinationExists("b.md".into()).into();
        assert_eq!(ApplyError::from_anyhow(&e).code, "destination-exists");

        let e: anyhow::Error = crate::delete_doc::DeletePreflightError::RewriteToSelf.into();
        assert_eq!(ApplyError::from_anyhow(&e).code, "rewrite-to-self");

        let e: anyhow::Error =
            crate::planner::intent::rewrite_wikilink::RewriteWikilinkError::OldUnresolved(
                "old".into(),
            )
            .into();
        assert_eq!(ApplyError::from_anyhow(&e).code, "target-not-found");
    }

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
    fn reconstruct_accepts_refusal_error_on_first_class_precondition() {
        let structured = serde_json::json!({
            "report": {
                "schema_version": 3,
                "trace_id": "",
                "plan_hash": "h",
                "vault_root": "/v",
                "dry_run": false,
                "applied": 0,
                "skipped": 0,
                "failed": 0,
                "remaining": 1,
                "preconditions": [{
                    "id": "task-owner",
                    "status": "failed",
                    "expected_paths": [],
                    "actual_paths": ["other/task.md"],
                    "error": {
                        "code": "owner-set-mismatch",
                        "message": "owner set changed"
                    }
                }],
                "operations": [{
                    "op_id": "0",
                    "kind": "create_document",
                    "status": "not-run",
                    "summary": "would create_document task.md"
                }],
                "outcome": "refused"
            }
        });

        let report = reconstruct_wire_report(&structured).expect("precondition refusal rebuilds");
        assert_eq!(report.outcome, ApplyOutcome::Refused);
        assert_eq!(report.operations[0].status, OpStatus::NotRun);
        assert_eq!(
            report.preconditions[0].error.as_ref().unwrap().code,
            "owner-set-mismatch"
        );
    }

    #[test]
    fn apply_report_serializes_with_per_op_status() {
        let report = ApplyReport {
            schema_version: APPLY_REPORT_SCHEMA_VERSION,
            trace_id: "".into(),
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

    /// NRN-150: the error envelope carries `path` only when present, and
    /// `ApplyError::from_rich` maps a rich apply error to its kebab code + path.
    #[test]
    fn error_envelope_shape_and_from_rich() {
        let rich = crate::standards::apply::ApplyError::StaleDocumentHash {
            path: "a.md".into(),
            expected: "aaa".into(),
            actual: "bbb".into(),
        };
        let env = ApplyError::from_rich(&rich);
        assert_eq!(env.code, "stale-document-hash");
        assert_eq!(env.path.as_deref(), Some("a.md"));
        let json = serde_json::to_value(&env).unwrap();
        assert_eq!(json["code"], "stale-document-hash");
        assert_eq!(json["path"], "a.md");

        // A pathless error omits the `path` key entirely.
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
        };
        let bare_json = serde_json::to_value(&bare).unwrap();
        assert!(bare_json.get("cascade").is_none());
    }

    /// NRN-175: the additive `path` / `stem` fields serialize only when
    /// populated (`skip_serializing_if = Option::is_none`), so an op with no
    /// natural resolved path stays byte-identical to the pre-NRN-175 shape.
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

    /// NRN-237: `link_impact` serializes only when populated (a `delete_document`
    /// op), so a sibling verb's op stays byte-identical to the pre-NRN-237 shape;
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
