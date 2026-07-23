//! The `delete` execute seam: remove a document and either leave its incoming
//! links broken (`--allow-broken-links`) or redirect them (`--rewrite-to`).
//!
//! The stem-resolving preflight — target resolution, the backlink policy refusal,
//! and `--rewrite-to` validation — is reproduced here so a clean pre-write
//! decline returns a coded `outcome = refused` [`ApplyReport`]. The plan is one
//! `delete_document` op carrying `rewrite_to` (when redirecting); the applier
//! reads it, cascades the redirect, then deletes.

use super::{owner_index_options, MutationExecution};
use crate::apply::{apply_migration_plan, ApplyContext};
use crate::domain::GraphIndex;
use crate::target::{backlinks, resolve_target_path};
use camino::Utf8PathBuf;
use norn_wire::{ApplyError, ApplyOutcome, ApplyReport};
use norn_wire::{MigrationOp, MigrationPlan, MIGRATION_PLAN_SCHEMA_VERSION};
use serde_json::{Map, Value};

/// Execute a `delete`: forecast (`confirm == false`) or apply (`confirm == true`).
pub fn execute(
    cache: &crate::cache::Cache,
    config: Option<&crate::standards::VaultConfig>,
    params: &norn_wire::DeleteParams,
    _today: &str,
    sink: &mut crate::telemetry::EventSink,
) -> anyhow::Result<MutationExecution<ApplyReport>> {
    let index = cache.load_graph_index()?;
    let vault_root = cache.vault_root().to_string();
    let dry_run = !params.confirm;

    // Preflight validates (and resolves) the redirect target — that resolution is
    // the refusal gate (rewrite-to-self / not-found / ambiguous) — but the PLAN
    // carries the RAW argument, so the resolved value is discarded here.
    let (doc_rel, _resolved_rewrite_to) = match preflight(&index, params) {
        Ok(pair) => pair,
        Err(refusal) => return Ok(refused(vault_root, dry_run, refusal)),
    };

    // This field set IS the wire contract (the `plan_hash` is the plan's
    // `canonical_hash()`): `path` is the
    // RESOLVED target, and `rewrite_to` is the RAW argument, always present as a
    // key (JSON `null` when absent) — NOT the resolved path, and never omitted.
    // The intent expander's `as_str()` reads a
    // `null` as "no redirect", and classify_link_risk rewrites stem backlinks to
    // the new stem identically whether the arg is a stem or a path, so post-state
    // is unaffected; only the hashed field value must match.
    // Stamp the REQUIRED plan-time compare-and-swap hash (ADR 0024 / NRN-151)
    // from the index loaded at plan synthesis — the delete pass fingerprint-CASs
    // the file bytes before removal. The target was just resolved out of this
    // same index, so its hash is present.
    let doc_hash = index
        .documents
        .iter()
        .find(|d| d.path == doc_rel)
        .map(|d| d.hash.clone());
    let op = MigrationOp {
        kind: "delete_document".into(),
        id: None,
        requires: Vec::new(),
        fields: delete_fields(&doc_rel, params, doc_hash.as_deref()),
        footnote: None,
    };
    let plan = MigrationPlan {
        schema_version: MIGRATION_PLAN_SCHEMA_VERSION,
        vault_root: vault_root.clone(),
        generator: None,
        generated_at: None,
        preconditions: Vec::new(),
        operations: vec![op],
        skipped: Vec::new(),
        plan_footnote: None,
    };

    let ctx = ApplyContext {
        dry_run,
        parents: false,
        verbose: false,
        refuse_as_report: true,
        owner_index_options: owner_index_options(config),
    };
    let apply_report = apply_migration_plan(&plan, &index, ctx, sink)?;
    // A forecast commits nothing (matches set/new).
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

/// Build the `delete_document` op fields. Field set (`mcp/tools/delete.rs`):
/// `path` (RESOLVED target), `rewrite_to` (the RAW arg, always present as a key —
/// JSON `null` when absent, NOT the resolved path), `allow_broken_links` (always
/// present), and `document_hash` — the REQUIRED plan-time CAS precondition (ADR
/// 0024 / NRN-151) — added when the target resolves in the index (always, for a
/// verb delete). This field set feeds `MigrationPlan::canonical_hash()`, so
/// `document_hash` participates in the plan hash (ADR 0024). `rewrite_to` as a
/// stem-or-path does not change
/// the cascade (classify_link_risk rewrites to the new stem either way).
fn delete_fields(
    doc_rel: &camino::Utf8Path,
    params: &norn_wire::DeleteParams,
    document_hash: Option<&str>,
) -> Value {
    let mut fields = Map::new();
    fields.insert("path".into(), Value::String(doc_rel.to_string()));
    fields.insert(
        "rewrite_to".into(),
        match &params.rewrite_to {
            Some(alt) => Value::String(alt.clone()),
            None => Value::Null,
        },
    );
    fields.insert(
        "allow_broken_links".into(),
        Value::Bool(params.allow_broken_links),
    );
    if let Some(hash) = document_hash {
        fields.insert("document_hash".into(), Value::String(hash.to_string()));
    }
    Value::Object(fields)
}

/// A coded delete preflight refusal — the `DeletePreflightError` codes +
/// Display prose are the wire contract.
struct DeleteRefusal {
    code: &'static str,
    message: String,
}

/// Resolve the target + optional redirect and run the ordered barriers,
/// returning `(target, rewrite_to?)` resolved vault-relative paths or a refusal.
fn preflight(
    index: &GraphIndex,
    params: &norn_wire::DeleteParams,
) -> Result<(Utf8PathBuf, Option<Utf8PathBuf>), DeleteRefusal> {
    let doc_rel = resolve_target_path(index, &params.target).map_err(|e| {
        if e.to_string().contains("ambiguous") {
            DeleteRefusal {
                code: "target-ambiguous",
                message: format!(
                    "document resolves ambiguously by stem: {} → {:?}",
                    params.target,
                    Vec::<Utf8PathBuf>::new()
                ),
            }
        } else {
            DeleteRefusal {
                code: "target-not-found",
                message: format!("document does not exist: {}", params.target),
            }
        }
    })?;

    let incoming = backlinks(index, &doc_rel);

    let rewrite_to_rel = match &params.rewrite_to {
        Some(alt) => {
            let alt_rel = resolve_target_path(index, alt).map_err(|e| {
                if e.to_string().contains("ambiguous") {
                    DeleteRefusal {
                        code: "rewrite-to-ambiguous",
                        message: format!(
                            "rewrite-to target resolves ambiguously by stem: {} → {:?}",
                            alt,
                            Vec::<Utf8PathBuf>::new()
                        ),
                    }
                } else {
                    DeleteRefusal {
                        code: "rewrite-to-not-found",
                        message: format!("rewrite-to target does not exist: {alt}"),
                    }
                }
            })?;
            if alt_rel == doc_rel {
                return Err(DeleteRefusal {
                    code: "rewrite-to-self",
                    message: "rewrite-to target resolves to the same document as target".into(),
                });
            }
            Some(alt_rel)
        }
        None => None,
    };

    // Incoming links with neither flag → refuse (the delete-specific policy).
    if !incoming.is_empty() && rewrite_to_rel.is_none() && !params.allow_broken_links {
        return Err(DeleteRefusal {
            code: "backlinks-present",
            message: format!(
                "document has {} incoming link(s); pass --allow-broken-links to accept, or --rewrite-to <ALT_DOC> to redirect",
                incoming.len()
            ),
        });
    }

    Ok((doc_rel, rewrite_to_rel))
}

fn refused(vault_root: String, dry_run: bool, r: DeleteRefusal) -> MutationExecution<ApplyReport> {
    let report = ApplyReport::refused(
        vault_root,
        dry_run,
        "delete_document",
        ApplyError {
            code: r.code.into(),
            message: r.message,
            path: None,
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

    fn params(target: &str, confirm: bool) -> norn_wire::DeleteParams {
        norn_wire::DeleteParams {
            target: target.into(),
            confirm,
            ..Default::default()
        }
    }

    // Field-set contract: `path` (resolved),
    // `rewrite_to` (RAW arg, null when None), `allow_broken_links` (always
    // present), `document_hash` when the target resolves (ADR 0024). Pins the
    // plan_hash contract for `--format json`.
    #[test]
    fn delete_fields_carry_raw_rewrite_to_null_and_allow_broken() {
        let none = norn_wire::DeleteParams {
            target: "b".into(),
            ..Default::default()
        };
        // No hash (target absent from index): document_hash omitted.
        assert_eq!(
            delete_fields(camino::Utf8Path::new("b.md"), &none, None),
            serde_json::json!({"path": "b.md", "rewrite_to": null, "allow_broken_links": false}),
            "rewrite_to is null (present) when absent; allow_broken_links always present"
        );

        let with = norn_wire::DeleteParams {
            target: "b".into(),
            rewrite_to: Some("c".into()),
            allow_broken_links: false,
            confirm: true,
        };
        assert_eq!(
            delete_fields(camino::Utf8Path::new("b.md"), &with, Some("beefcafe")),
            serde_json::json!({"path": "b.md", "rewrite_to": "c", "allow_broken_links": false, "document_hash": "beefcafe"}),
            "rewrite_to carries the RAW arg (not the resolved path); the plan-time CAS hash rides when resolved; confirm is not a plan field"
        );
    }

    #[test]
    fn missing_target_refuses() {
        let (_t, root) = synth_vault(&[("a.md", "---\ntype: note\n---\n# A\n")]);
        let cache = built(&root);
        let exec = execute(&cache, None, &params("nope", true), TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, ApplyOutcome::Refused);
        assert_eq!(
            exec.report.operations[0].error.as_ref().unwrap().code,
            "target-not-found"
        );
    }

    #[test]
    fn incoming_links_without_flag_refuses() {
        let (_t, root) = synth_vault(&[
            ("a.md", "---\ntype: note\n---\n# A\n[[b]]\n"),
            ("b.md", "---\ntype: note\n---\n# B\n"),
        ]);
        let cache = built(&root);
        let exec = execute(&cache, None, &params("b.md", true), TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, ApplyOutcome::Refused);
        assert_eq!(
            exec.report.operations[0].error.as_ref().unwrap().code,
            "backlinks-present"
        );
    }

    #[test]
    fn rewrite_to_self_refuses() {
        let (_t, root) = synth_vault(&[
            ("a.md", "---\ntype: note\n---\n# A\n[[b]]\n"),
            ("b.md", "---\ntype: note\n---\n# B\n"),
        ]);
        let cache = built(&root);
        let mut p = params("b.md", true);
        p.rewrite_to = Some("b.md".into());
        let exec = execute(&cache, None, &p, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, ApplyOutcome::Refused);
        assert_eq!(
            exec.report.operations[0].error.as_ref().unwrap().code,
            "rewrite-to-self"
        );
    }

    #[test]
    fn apply_with_rewrite_to_redirects_and_deletes() {
        let (_t, root) = synth_vault(&[
            ("a.md", "---\ntype: note\n---\n# A\n[[b]]\n"),
            ("b.md", "---\ntype: note\n---\n# B\n"),
            ("c.md", "---\ntype: note\n---\n# C\n"),
        ]);
        let cache = built(&root);
        let mut p = params("b.md", true);
        p.rewrite_to = Some("c.md".into());
        let exec = execute(&cache, None, &p, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, ApplyOutcome::Applied);
        assert!(!root.join("b.md").as_std_path().exists());
        let a = std::fs::read_to_string(root.join("a.md").as_std_path()).unwrap();
        assert!(a.contains("[[c]]"), "backlink redirected: {a}");
        assert!(!exec.touched_paths.is_empty());
    }

    #[test]
    fn apply_allow_broken_deletes_and_leaves_links() {
        let (_t, root) = synth_vault(&[
            ("a.md", "---\ntype: note\n---\n# A\n[[b]]\n"),
            ("b.md", "---\ntype: note\n---\n# B\n"),
        ]);
        let cache = built(&root);
        let mut p = params("b.md", true);
        p.allow_broken_links = true;
        let exec = execute(&cache, None, &p, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, ApplyOutcome::Applied);
        assert!(!root.join("b.md").as_std_path().exists());
        let a = std::fs::read_to_string(root.join("a.md").as_std_path()).unwrap();
        assert!(a.contains("[[b]]"), "link left broken: {a}");
    }
}
