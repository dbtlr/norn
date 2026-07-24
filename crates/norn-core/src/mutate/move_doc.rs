//! The `move` execute seam: relocate a document (or, with `recursive`, a folder)
//! and cascade-rewrite the backlinks that point at it.
//!
//! The stem-resolving preflight (source resolution, same-path, parent, destination)
//! is reproduced here so a clean pre-write decline returns a coded
//! `outcome = refused` [`ApplyReport`] — never a bare `Err`. A resolved single
//! move builds a one-op `move_document` `MigrationPlan`; a folder move builds a
//! `move_folder` op the planner expands to N `move_document` ops. The applier
//! reads `link_risk` off the `move_document` op and drives the cascade — no
//! separate rewrite ops.

use super::{owner_index_options, MutationExecution};
use crate::apply::{apply_migration_plan, ApplyContext};
use crate::domain::GraphIndex;
use crate::target::{resolve_target, target_refusal, TargetRefusalFamily, TargetResolution};
use camino::Utf8PathBuf;
use norn_wire::{ApplyError, ApplyOutcome, ApplyReport};
use norn_wire::{MigrationOp, MigrationPlan, MIGRATION_PLAN_SCHEMA_VERSION};
use serde_json::Value;

/// Execute a `move`: forecast (`confirm == false`) or apply (`confirm == true`).
pub fn execute(
    cache: &crate::cache::Cache,
    config: Option<&crate::standards::VaultConfig>,
    params: &norn_wire::MoveParams,
    _today: &str,
    sink: &mut crate::telemetry::EventSink,
) -> anyhow::Result<MutationExecution<ApplyReport>> {
    let index = cache.load_graph_index()?;
    let vault_root = cache.vault_root().to_owned();
    let dry_run = !params.confirm;

    // Folder move: the `--recursive` flag, or a source that names a directory on
    // disk (this plans a `move_folder` op the planner expands). Otherwise a
    // single-document move with the stem-resolving preflight.
    let src_abs = vault_root.join(&params.from);
    let is_folder = params.recursive || src_abs.as_std_path().is_dir();

    let plan = if is_folder {
        let op = MigrationOp {
            kind: "move_folder".into(),
            id: None,
            requires: Vec::new(),
            fields: folder_move_fields(params),
            footnote: None,
        };
        one_op_plan(vault_root.to_string(), op)
    } else {
        // ── Single-file preflight ─────────────────────────────────────────────
        let resolved_src = match preflight_single(&index, &vault_root, params) {
            Ok(src) => src,
            Err(refusal) => {
                return Ok(refused(vault_root.to_string(), dry_run, refusal));
            }
        };
        // Stamp the plan-time compare-and-swap hash from the index loaded at plan
        // synthesis (ADR 0024) — the move pass fingerprint-checks the source
        // before renaming. The source was just resolved out of this same index,
        // so its hash is present.
        let src_hash = index
            .documents
            .iter()
            .find(|d| d.path == resolved_src)
            .map(|d| d.hash.clone());
        let op = MigrationOp {
            kind: "move_document".into(),
            id: None,
            requires: Vec::new(),
            fields: single_move_fields(&resolved_src, params, src_hash.as_deref()),
            footnote: None,
        };
        one_op_plan(vault_root.to_string(), op)
    };

    let ctx = ApplyContext {
        dry_run,
        parents: params.parents,
        verbose: false,
        refuse_as_report: true,
        owner_index_options: owner_index_options(config),
    };
    let apply_report = apply_migration_plan(&plan, &index, ctx, sink)?;
    // A forecast commits nothing (matches set/new): only a confirmed apply hands
    // the owner touched paths for the cache-increment commit.
    let touched_paths = if params.confirm {
        apply_report.touched_paths.clone()
    } else {
        Vec::new()
    };
    Ok(MutationExecution {
        report: apply_report,
        touched_paths,
    })
}

/// Build the `move_document` op fields. `src` (resolved) / `dst` / `parents` are
/// ALWAYS present; `force` and `no_link_rewrite` are added ONLY when set.
/// `document_hash` — the plan-time CAS precondition (ADR 0024) — is added when
/// the source resolves in the index (always, for a verb move). This field set
/// feeds `MigrationPlan::canonical_hash()`, so `document_hash` participates in
/// the plan hash (ADR 0024).
fn single_move_fields(
    resolved_src: &camino::Utf8Path,
    params: &norn_wire::MoveParams,
    document_hash: Option<&str>,
) -> Value {
    let mut fields = serde_json::Map::new();
    fields.insert("src".into(), Value::String(resolved_src.to_string()));
    fields.insert("dst".into(), Value::String(params.to.clone()));
    fields.insert("parents".into(), Value::Bool(params.parents));
    if params.force {
        fields.insert("force".into(), Value::Bool(true));
    }
    if params.no_link_rewrite {
        fields.insert("no_link_rewrite".into(), Value::Bool(true));
    }
    if let Some(hash) = document_hash {
        fields.insert("document_hash".into(), Value::String(hash.to_string()));
    }
    Value::Object(fields)
}

/// Build the `move_folder` op fields. `src` / `dst` / `parents` are ALWAYS
/// present; `force` and `no_link_rewrite` are added ONLY when set — mirroring
/// `single_move_fields` so a flagless folder move hashes identically to before
/// the flags were threaded (ADR 0024). The planner reads both off the decoded
/// [`MoveFolderFields`](norn_wire::MoveFolderFields) and propagates them to every
/// expanded per-document op.
fn folder_move_fields(params: &norn_wire::MoveParams) -> Value {
    let mut fields = serde_json::Map::new();
    fields.insert("src".into(), Value::String(params.from.clone()));
    fields.insert("dst".into(), Value::String(params.to.clone()));
    fields.insert("parents".into(), Value::Bool(params.parents));
    if params.force {
        fields.insert("force".into(), Value::Bool(true));
    }
    if params.no_link_rewrite {
        fields.insert("no_link_rewrite".into(), Value::Bool(true));
    }
    Value::Object(fields)
}

/// A coded single-file move preflight refusal — the `MovePreflightError`
/// codes + Display prose are the wire contract.
struct MoveRefusal {
    code: &'static str,
    message: String,
    path: Option<String>,
}

/// Resolve the source and run the ordered preflight barriers, returning
/// the resolved vault-relative source path (planned, never the raw token) or a
/// coded refusal.
fn preflight_single(
    index: &GraphIndex,
    vault_root: &camino::Utf8Path,
    params: &norn_wire::MoveParams,
) -> Result<Utf8PathBuf, MoveRefusal> {
    let src_rel = match resolve_target(index, &params.from) {
        TargetResolution::Resolved(path) => path,
        TargetResolution::NotFound => {
            let (code, message) = target_refusal(
                TargetRefusalFamily::NotFound,
                format!("source does not exist: {}", params.from),
            );
            return Err(MoveRefusal {
                code,
                message,
                path: None,
            });
        }
        TargetResolution::Ambiguous(candidates) => {
            let (code, message) = target_refusal(
                TargetRefusalFamily::Ambiguous,
                format!(
                    "source resolves ambiguously by stem: {} → {candidates:?}",
                    params.from
                ),
            );
            return Err(MoveRefusal {
                code,
                message,
                path: None,
            });
        }
    };
    let dst_rel = Utf8PathBuf::from(&params.to);

    // Same-path (no-op) BEFORE the existence check so `--force` cannot silence it.
    let src_abs = vault_root.join(&src_rel);
    let dst_abs = vault_root.join(&dst_rel);
    let src_canon = src_abs
        .as_std_path()
        .canonicalize()
        .ok()
        .and_then(|p| Utf8PathBuf::from_path_buf(p).ok());
    let dst_canon = dst_abs
        .as_std_path()
        .canonicalize()
        .ok()
        .and_then(|p| Utf8PathBuf::from_path_buf(p).ok());
    let same = match (src_canon, dst_canon) {
        (Some(s), Some(d)) => s == d,
        _ => src_rel == dst_rel,
    };
    if same {
        return Err(MoveRefusal {
            code: "source-destination-same",
            message: format!(
                "source and destination resolve to the same canonical path: {src_rel}"
            ),
            path: Some(src_rel.to_string()),
        });
    }

    // Destination parent must exist unless `--parents` (the applier creates it).
    if !params.parents {
        if let Some(parent) = dst_rel.parent() {
            if !parent.as_str().is_empty() && !vault_root.join(parent).as_std_path().exists() {
                return Err(MoveRefusal {
                    code: "parent-missing",
                    message: format!("destination parent directory does not exist: {parent}"),
                    path: Some(dst_rel.to_string()),
                });
            }
        }
    }

    // Destination must not already exist unless `--force`.
    if dst_abs.as_std_path().exists() && !params.force {
        return Err(MoveRefusal {
            code: "destination-exists",
            message: format!("destination already exists: {dst_rel} (pass --force to overwrite)"),
            path: Some(dst_rel.to_string()),
        });
    }

    Ok(src_rel)
}

fn one_op_plan(vault_root: String, op: MigrationOp) -> MigrationPlan {
    MigrationPlan {
        schema_version: MIGRATION_PLAN_SCHEMA_VERSION,
        vault_root,
        generator: None,
        generated_at: None,
        preconditions: Vec::new(),
        operations: vec![op],
        skipped: Vec::new(),
        plan_footnote: None,
    }
}

fn refused(vault_root: String, dry_run: bool, r: MoveRefusal) -> MutationExecution<ApplyReport> {
    let report = ApplyReport::refused(
        vault_root,
        dry_run,
        "move_document",
        ApplyError {
            code: r.code.into(),
            message: r.message,
            path: r.path,
        },
    );
    debug_assert_eq!(report.outcome, ApplyOutcome::Refused);
    MutationExecution {
        report,
        touched_paths: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    const TODAY: &str = "2026-07-20";

    fn sink() -> crate::telemetry::EventSink {
        crate::telemetry::EventSink::discard(
            crate::telemetry::IdGen::with_seed(0),
            crate::telemetry::Clock::fixed("2026-07-20T00:00:00.000Z"),
        )
    }

    fn synth_vault(docs: &[(&str, &str)]) -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::create_dir(root.join(".norn").as_std_path()).unwrap();
        std::fs::write(
            root.join(".norn/config.yaml").as_std_path(),
            "validate: {}\n",
        )
        .unwrap();
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

    fn params(from: &str, to: &str, confirm: bool) -> norn_wire::MoveParams {
        norn_wire::MoveParams {
            from: from.into(),
            to: to.into(),
            confirm,
            ..Default::default()
        }
    }

    // Field-set contract: `src`/`dst`/`parents` always present;
    // `force`/`no_link_rewrite` present only when set; `document_hash` present
    // when the source resolves in the index (ADR 0024). Pins the plan_hash
    // contract for `--format json`.
    #[test]
    fn move_fields_omit_false_force_and_no_link_rewrite() {
        let p = norn_wire::MoveParams {
            from: "b".into(),
            to: "renamed.md".into(),
            ..Default::default()
        };
        // No hash resolved (source absent from index): document_hash omitted.
        let f = single_move_fields(camino::Utf8Path::new("notes/b.md"), &p, None);
        assert_eq!(
            f,
            serde_json::json!({"src": "notes/b.md", "dst": "renamed.md", "parents": false}),
            "false force/no_link_rewrite must be omitted; parents always present"
        );
    }

    #[test]
    fn move_fields_stamp_document_hash_when_present() {
        let p = norn_wire::MoveParams {
            from: "b".into(),
            to: "renamed.md".into(),
            ..Default::default()
        };
        let f = single_move_fields(camino::Utf8Path::new("notes/b.md"), &p, Some("cafef00d"));
        assert_eq!(
            f,
            serde_json::json!({
                "src": "notes/b.md",
                "dst": "renamed.md",
                "parents": false,
                "document_hash": "cafef00d",
            }),
            "the plan-time CAS hash rides the move op fields (ADR 0024)"
        );
    }

    #[test]
    fn move_fields_include_set_flags() {
        let p = norn_wire::MoveParams {
            from: "b".into(),
            to: "renamed.md".into(),
            parents: true,
            force: true,
            no_link_rewrite: true,
            ..Default::default()
        };
        let f = single_move_fields(camino::Utf8Path::new("notes/b.md"), &p, Some("deadbeef"));
        assert_eq!(
            f,
            serde_json::json!({
                "src": "notes/b.md",
                "dst": "renamed.md",
                "parents": true,
                "force": true,
                "no_link_rewrite": true,
                "document_hash": "deadbeef",
            })
        );
    }

    #[test]
    fn source_missing_refuses() {
        let (_t, root) = synth_vault(&[("a.md", "---\ntype: note\n---\n# A\n")]);
        let cache = built(&root);
        let exec = execute(
            &cache,
            None,
            &params("nope", "b.md", false),
            TODAY,
            &mut sink(),
        )
        .unwrap();
        assert_eq!(exec.report.outcome, ApplyOutcome::Refused);
        assert_eq!(
            exec.report.operations[0].error.as_ref().unwrap().code,
            "target-not-found"
        );
        assert!(exec.touched_paths.is_empty());
    }

    #[test]
    fn same_path_refuses_even_with_force() {
        let (_t, root) = synth_vault(&[("a.md", "---\ntype: note\n---\n# A\n")]);
        let cache = built(&root);
        let mut p = params("a.md", "a.md", true);
        p.force = true;
        let exec = execute(&cache, None, &p, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, ApplyOutcome::Refused);
        assert_eq!(
            exec.report.operations[0].error.as_ref().unwrap().code,
            "source-destination-same"
        );
    }

    #[test]
    fn destination_exists_refuses_without_force() {
        let (_t, root) = synth_vault(&[
            ("a.md", "---\ntype: note\n---\n# A\n"),
            ("b.md", "---\ntype: note\n---\n# B\n"),
        ]);
        let cache = built(&root);
        let exec = execute(
            &cache,
            None,
            &params("a.md", "b.md", false),
            TODAY,
            &mut sink(),
        )
        .unwrap();
        assert_eq!(exec.report.outcome, ApplyOutcome::Refused);
        assert_eq!(
            exec.report.operations[0].error.as_ref().unwrap().code,
            "destination-exists"
        );
    }

    #[test]
    fn apply_moves_and_rewrites_backlink() {
        let (_t, root) = synth_vault(&[
            ("a.md", "---\ntype: note\n---\n# A\n[[b]]\n"),
            ("b.md", "---\ntype: note\n---\n# B\n"),
        ]);
        let cache = built(&root);
        let exec = execute(
            &cache,
            None,
            &params("b.md", "renamed.md", true),
            TODAY,
            &mut sink(),
        )
        .unwrap();
        assert_eq!(exec.report.outcome, ApplyOutcome::Applied);
        assert!(root.join("renamed.md").as_std_path().exists());
        assert!(!root.join("b.md").as_std_path().exists());
        // The backlink in a.md was cascade-rewritten to the new stem.
        let a = std::fs::read_to_string(root.join("a.md").as_std_path()).unwrap();
        assert!(a.contains("[[renamed]]"), "backlink rewritten: {a}");
        assert!(!exec.touched_paths.is_empty());
    }

    #[test]
    fn dry_run_writes_nothing() {
        let (_t, root) = synth_vault(&[
            ("a.md", "---\ntype: note\n---\n# A\n[[b]]\n"),
            ("b.md", "---\ntype: note\n---\n# B\n"),
        ]);
        let cache = built(&root);
        let exec = execute(
            &cache,
            None,
            &params("b.md", "renamed.md", false),
            TODAY,
            &mut sink(),
        )
        .unwrap();
        assert_eq!(exec.report.outcome, ApplyOutcome::Applied);
        assert!(exec.report.dry_run);
        assert!(root.join("b.md").as_std_path().exists());
        assert!(!root.join("renamed.md").as_std_path().exists());
        assert!(exec.touched_paths.is_empty());
    }
}
