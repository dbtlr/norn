//! The `edit` execute seam: atomic, content-anchored body edits, built as a
//! single `replace_body` `MigrationPlan` op and applied through the shared
//! `apply_migration_plan` executor.
//!
//! The ops arrive
//! already resolved (CLI-side sugar-desugar or `--edits-json`/`--ops-file`/stdin
//! parse), re-serialized onto the wire as a JSON array; this seam decodes them,
//! runs the pure [`apply_edits`](crate::edit::transform::apply_edits) transform
//! against the target's current body, and — when the body changed — stamps the
//! resulting body as one `replace_body` op. Every clean pre-write decline (a bad
//! target, a malformed op, an anchor miss, a content-hash drift) returns a
//! `Refused` report carrying a coded error, never a bare `Err`.

use super::{owner_index_options, MutationExecution};
use crate::apply::{apply_migration_plan, ApplyContext};
use crate::edit::ops::EditOp;
use crate::edit::transform::apply_edits;
use norn_wire::{ApplyOutcome, OpStatus};
use norn_wire::{
    CodedError, EditChange, EditParams, EditReport, MutationOutcome, EDIT_REPORT_SCHEMA_VERSION,
};
use norn_wire::{MigrationOp, MigrationPlan, MIGRATION_PLAN_SCHEMA_VERSION};
use serde_json::{Map, Value};

/// Execute an `edit`: forecast (`confirm == false`) or apply (`confirm == true`).
pub fn execute(
    cache: &crate::cache::Cache,
    config: Option<&crate::standards::VaultConfig>,
    params: &EditParams,
    _today: &str,
    sink: &mut crate::telemetry::EventSink,
) -> anyhow::Result<MutationExecution<EditReport>> {
    let index = cache.load_graph_index()?;
    let vault_root = cache.vault_root().to_string();

    // ── Target resolution (refusal prose is end-user contract; mirrors `set`) ──
    let target_path = match crate::target::resolve_target(&index, &params.target) {
        crate::target::TargetResolution::Resolved(p) => p,
        crate::target::TargetResolution::NotFound => {
            let (code, msg) = crate::target::target_refusal(
                crate::target::TargetRefusalFamily::NotFound,
                format!("doc not found: {}", params.target),
            );
            return Ok(refused(params.target.clone(), code, msg, None));
        }
        crate::target::TargetResolution::Ambiguous(candidates) => {
            let (code, msg) = crate::target::target_refusal(
                crate::target::TargetRefusalFamily::Ambiguous,
                format!(
                    "ambiguous document stem: {}; candidates: {}",
                    params.target,
                    candidates
                        .iter()
                        .map(|path| path.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
            return Ok(refused(params.target.clone(), code, msg, None));
        }
    };
    let target_str = target_path.to_string();

    let doc = index
        .documents
        .iter()
        .find(|d| d.path == target_path)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("resolved target not in index: {target_path}"))?;

    // ── Decode the resolved ops array ────────────────────────────────────────
    // The CLI already parsed + validated these; a decode failure here is a
    // defensive refusal, not the ordinary path.
    let ops: Vec<EditOp> = match serde_json::from_str(&params.edits) {
        Ok(ops) => ops,
        Err(e) => {
            return Ok(refused(
                target_str,
                "invalid-edits",
                format!("invalid edits JSON: {e}"),
                None,
            ))
        }
    };
    if ops.is_empty() {
        return Ok(refused(
            target_str,
            "edits-empty",
            "edits array is empty",
            None,
        ));
    }

    // ── Opt-in compare-and-swap (NRN-220) ────────────────────────────────────
    // `doc.hash` is the full-content blake3 hex the index carries (the same
    // value `norn get`'s `.document_hash` facet exposes). Case-insensitive,
    // whitespace-trimmed: hex is case-agnostic, so an uppercase-copied hash is
    // not drift.
    if let Some(expected) = &params.expected_hash {
        if !doc.hash.eq_ignore_ascii_case(expected.trim()) {
            return Ok(refused(
                target_str.clone(),
                "stale-document-hash",
                format!(
                    "document {target_str} has drifted from the expected hash: expected {}, found {}",
                    expected.trim(),
                    doc.hash
                ),
                Some(target_str),
            ));
        }
    }

    // ── Pure transform ───────────────────────────────────────────────────────
    let transform = match apply_edits(&doc.body_text, &ops) {
        Ok(t) => t,
        Err(e) => {
            return Ok(refused(
                target_str.clone(),
                e.code(),
                e.to_string(),
                e.path().map(str::to_string),
            ))
        }
    };

    // ── Body-change bookkeeping ───────────────────────────────────────────────
    let old_len = doc.body_text.len();
    let body_changed = transform.new_body != doc.body_text;
    let body_bytes_old = Some(old_len);
    let body_bytes_new = if body_changed {
        Some(transform.new_body.len())
    } else {
        None
    };

    // ── Build the plan (a single replace_body op, only when the body changed) ─
    let mut operations: Vec<MigrationOp> = Vec::new();
    if body_changed {
        let mut fields = Map::new();
        fields.insert("path".into(), Value::String(target_str.clone()));
        fields.insert(
            "new_value".into(),
            Value::String(transform.new_body.clone()),
        );
        operations.push(MigrationOp {
            kind: "replace_body".to_string(),
            id: None,
            requires: Vec::new(),
            fields: Value::Object(fields),
            footnote: None,
        });
    }

    let plan = MigrationPlan {
        schema_version: MIGRATION_PLAN_SCHEMA_VERSION,
        vault_root,
        generator: None,
        generated_at: None,
        preconditions: Vec::new(),
        operations,
        skipped: Vec::new(),
        plan_footnote: None,
    };

    // ── Apply (forecast writes nothing) ──────────────────────────────────────
    let ctx = ApplyContext {
        dry_run: !params.confirm,
        parents: false,
        verbose: false,
        refuse_as_report: true,
        owner_index_options: owner_index_options(config),
    };
    let apply_report = apply_migration_plan(&plan, &index, ctx, sink)?;

    if matches!(
        apply_report.outcome,
        ApplyOutcome::Refused | ApplyOutcome::Failed
    ) {
        let coded = apply_report
            .operations
            .iter()
            .find(|o| o.status == OpStatus::Failed)
            .and_then(|o| o.error.clone())
            .map(|e| CodedError {
                code: e.code,
                message: e.message,
                path: e.path,
            })
            .unwrap_or_else(|| CodedError {
                code: "internal-error".into(),
                message: "apply refused without a coded op error".into(),
                path: None,
            });
        return Ok(MutationExecution {
            report: refused_report(target_str, coded),
            touched_paths: Vec::new(),
        });
    }

    let applied = params.confirm;
    let touched_paths = if applied {
        apply_report.touched_paths.clone()
    } else {
        Vec::new()
    };

    let edits: Vec<EditChange> = transform
        .descriptors
        .iter()
        .map(|d| EditChange {
            op: d.op.clone(),
            anchor: d.anchor_desc.clone(),
            matched: true,
            occurrences: d.occurrences,
            applied,
        })
        .collect();

    Ok(MutationExecution {
        report: EditReport {
            schema_version: EDIT_REPORT_SCHEMA_VERSION,
            // Real trace id on a confirmed apply, empty on a forecast (NRN-400);
            // the executor mints it from the EventSink on a write (see
            // `mutate::set::execute`).
            trace_id: apply_report.trace_id.clone(),
            telemetry_degraded: apply_report.telemetry_degraded,
            operation: "edit".into(),
            target: target_str,
            edits,
            body_changed,
            body_bytes_old,
            body_bytes_new,
            applied,
            outcome: if applied {
                MutationOutcome::Applied
            } else {
                MutationOutcome::Forecast
            },
            error: None,
        },
        touched_paths,
    })
}

/// A coded pre-write refusal report.
fn refused(
    target: impl Into<String>,
    code: &str,
    message: impl Into<String>,
    path: Option<String>,
) -> MutationExecution<EditReport> {
    MutationExecution {
        report: refused_report(
            target.into(),
            CodedError {
                code: code.into(),
                message: message.into(),
                path,
            },
        ),
        touched_paths: Vec::new(),
    }
}

fn refused_report(target: String, error: CodedError) -> EditReport {
    EditReport {
        schema_version: EDIT_REPORT_SCHEMA_VERSION,
        trace_id: String::new(),
        telemetry_degraded: false,
        operation: "edit".into(),
        target,
        edits: Vec::new(),
        body_changed: false,
        body_bytes_old: None,
        body_bytes_new: None,
        applied: false,
        outcome: MutationOutcome::Refused,
        error: Some(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    const TODAY: &str = "2026-07-19";

    fn sink() -> crate::telemetry::EventSink {
        crate::telemetry::EventSink::discard(
            crate::telemetry::IdGen::with_seed(0),
            crate::telemetry::Clock::fixed("2026-07-19T00:00:00.000Z"),
        )
    }

    fn synth_vault(docs: &[(&str, &str)]) -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        for (path, contents) in docs {
            let full = root.join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent.as_std_path()).unwrap();
            }
            std::fs::write(full.as_std_path(), contents).unwrap();
        }
        (tmp, root)
    }

    fn built(root: &Utf8PathBuf) -> crate::cache::Cache {
        let mut cache = crate::cache::Cache::open(root).unwrap();
        cache.full_build(root).unwrap();
        cache
    }

    fn edit_params(target: &str, edits: &str, confirm: bool) -> EditParams {
        EditParams {
            target: target.into(),
            edits: edits.into(),
            expected_hash: None,
            confirm,
        }
    }

    #[test]
    fn str_replace_forecast_writes_nothing() {
        let (_t, root) = synth_vault(&[("a.md", "---\ntype: note\n---\nhello world\n")]);
        let disk = root.join("a.md");
        let cache = built(&root);
        let params = edit_params(
            "a.md",
            r#"[{"op":"str_replace","old":"world","new":"norn"}]"#,
            false,
        );
        let exec = execute(&cache, None, &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Forecast);
        assert!(!exec.report.applied, "forecast");
        assert!(exec.report.body_changed);
        assert_eq!(exec.report.edits.len(), 1);
        assert_eq!(exec.report.edits[0].op, "str_replace");
        assert_eq!(exec.report.edits[0].occurrences, Some(1));
        assert!(exec.touched_paths.is_empty());
        assert!(std::fs::read_to_string(disk.as_std_path())
            .unwrap()
            .contains("hello world"));
    }

    #[test]
    fn str_replace_apply_writes_file() {
        let (_t, root) = synth_vault(&[("a.md", "---\ntype: note\n---\nhello world\n")]);
        let disk = root.join("a.md");
        let cache = built(&root);
        let params = edit_params(
            "a.md",
            r#"[{"op":"str_replace","old":"world","new":"norn"}]"#,
            true,
        );
        let exec = execute(&cache, None, &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Applied);
        assert!(exec.report.applied);
        assert!(!exec.touched_paths.is_empty());
        let on_disk = std::fs::read_to_string(disk.as_std_path()).unwrap();
        assert!(on_disk.contains("hello norn"));
        assert!(on_disk.contains("type: note"), "frontmatter preserved");
    }

    #[test]
    fn section_op_applies() {
        let (_t, root) =
            synth_vault(&[("a.md", "---\ntype: note\n---\n# Doc\n\n## Tasks\n- one\n")]);
        let cache = built(&root);
        let params = edit_params(
            "a.md",
            r#"[{"op":"append_to_section","heading":"Tasks","content":"- two"}]"#,
            true,
        );
        let exec = execute(&cache, None, &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Applied);
        assert_eq!(exec.report.edits[0].op, "append_to_section");
        assert!(exec.report.edits[0].occurrences.is_none());
        let on_disk = std::fs::read_to_string(root.join("a.md").as_std_path()).unwrap();
        assert!(on_disk.contains("- two"));
    }

    #[test]
    fn bad_target_refused() {
        let (_t, root) = synth_vault(&[("a.md", "---\ntype: note\n---\nbody\n")]);
        let cache = built(&root);
        let params = edit_params(
            "nonexistent",
            r#"[{"op":"str_replace","old":"a","new":"b"}]"#,
            false,
        );
        let exec = execute(&cache, None, &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Refused);
        assert_eq!(exec.report.error.as_ref().unwrap().code, "target-not-found");
    }

    #[test]
    fn anchor_not_found_refused() {
        let (_t, root) = synth_vault(&[("a.md", "---\ntype: note\n---\nbody\n")]);
        let cache = built(&root);
        let params = edit_params(
            "a.md",
            r#"[{"op":"str_replace","old":"nope","new":"x"}]"#,
            true,
        );
        let exec = execute(&cache, None, &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Refused);
        assert_eq!(exec.report.error.as_ref().unwrap().code, "anchor-not-found");
    }

    #[test]
    fn expected_hash_mismatch_refused() {
        let (_t, root) = synth_vault(&[("a.md", "---\ntype: note\n---\nbody\n")]);
        let cache = built(&root);
        let params = EditParams {
            target: "a.md".into(),
            edits: r#"[{"op":"str_replace","old":"body","new":"x"}]"#.into(),
            expected_hash: Some("deadbeef".into()),
            confirm: true,
        };
        let exec = execute(&cache, None, &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Refused);
        assert_eq!(
            exec.report.error.as_ref().unwrap().code,
            "stale-document-hash"
        );
    }

    #[test]
    fn empty_ops_refused() {
        let (_t, root) = synth_vault(&[("a.md", "---\ntype: note\n---\nbody\n")]);
        let cache = built(&root);
        let params = edit_params("a.md", "[]", false);
        let exec = execute(&cache, None, &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Refused);
        assert_eq!(exec.report.error.as_ref().unwrap().code, "edits-empty");
    }
}
