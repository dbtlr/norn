use std::fs;

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use vault_core::GraphIndex;
use vault_standards::apply::{
    apply_delete, apply_file_changes, apply_link_rewrites, apply_move, apply_rewrite_link,
    changes_by_path, validate_plan_for_apply, ApplyError, DeleteResult, LinkRewriteResult,
    MoveResult, RepairApplyWarning,
};
use vault_standards::{Finding, PlannedChange, RepairPlan};

#[allow(unused_imports)]
pub use vault_standards::apply::{
    RepairApplyPlanContext, RepairApplyReport, RepairApplyVerification,
};

fn check_hash(
    current_hashes: &std::collections::BTreeMap<Utf8PathBuf, String>,
    change: &PlannedChange,
) -> Result<()> {
    let current_hash = current_hashes.get(&change.path).ok_or_else(|| {
        anyhow::anyhow!(ApplyError::UnknownPath {
            path: change.path.clone(),
        })
    })?;
    if current_hash != &change.document_hash {
        return Err(anyhow::anyhow!(ApplyError::StaleDocumentHash {
            path: change.path.clone(),
            expected: change.document_hash.clone(),
            actual: current_hash.clone(),
        }));
    }
    Ok(())
}

pub fn apply_repair_plan(
    cwd: &Utf8PathBuf,
    index: &GraphIndex,
    plan: &RepairPlan,
    dry_run: bool,
) -> Result<RepairApplyReport> {
    validate_plan_for_apply(cwd, plan)?;

    // Pass 1: per-file frontmatter edits. changes_by_path skips
    // move_document, so the grouped map only contains set/remove/add
    // frontmatter changes.
    let grouped = changes_by_path(plan)?;

    let mut report = RepairApplyReport::new(plan, dry_run);

    let current_hashes: std::collections::BTreeMap<Utf8PathBuf, String> = index
        .documents
        .iter()
        .map(|d| (d.path.clone(), d.hash.clone()))
        .collect();

    for (rel_path, changes) in &grouped {
        // Hash check against the first change in the group (all share the same
        // document_hash for a given path — changes_by_path rejects mismatches).
        check_hash(&current_hashes, changes[0])?;

        let absolute_path = cwd.join(rel_path);
        let original =
            fs::read_to_string(&absolute_path).with_context(|| format!("read {absolute_path}"))?;
        let updated = apply_file_changes(&original, changes)?;

        if updated != original {
            report.changed_files.push(rel_path.clone());
            if !dry_run {
                fs::write(&absolute_path, updated)
                    .with_context(|| format!("write {absolute_path}"))?;
            }
        }
    }

    // Pass 1b: rewrite_link operations (broken wikilink target rewrites).
    // Hash check uses the index snapshot (same as Pass 1 frontmatter check).
    for change in plan
        .changes
        .iter()
        .filter(|c| c.operation == "rewrite_link")
    {
        check_hash(&current_hashes, change)?;

        if dry_run {
            // Record what would be rewritten without touching the file.
            if let (Some(from), Some(to)) = (
                change.expected_old_value.as_ref().and_then(|v| v.as_str()),
                change.new_value.as_ref().and_then(|v| v.as_str()),
            ) {
                report.rewritten_links.push(LinkRewriteResult {
                    file: change.path.clone(),
                    from: from.to_string(),
                    to: to.to_string(),
                });
                if !report.changed_files.contains(&change.path) {
                    report.changed_files.push(change.path.clone());
                }
            }
            continue;
        }

        let absolute_path = cwd.join(&change.path);
        let content =
            fs::read_to_string(&absolute_path).with_context(|| format!("read {absolute_path}"))?;
        let updated = apply_rewrite_link(&content, change)?;
        if updated != content {
            fs::write(&absolute_path, &updated)
                .with_context(|| format!("write {absolute_path}"))?;
            if !report.changed_files.contains(&change.path) {
                report.changed_files.push(change.path.clone());
            }
            if let (Some(from), Some(to)) = (
                change.expected_old_value.as_ref().and_then(|v| v.as_str()),
                change.new_value.as_ref().and_then(|v| v.as_str()),
            ) {
                report.rewritten_links.push(LinkRewriteResult {
                    file: change.path.clone(),
                    from: from.to_string(),
                    to: to.to_string(),
                });
            }
        }
    }

    // Pass 1c: delete_document operations.
    // Sequenced after rewrite_link so --rewrite-to redirects backlinks before
    // the target file disappears, and before move_document so delete-then-move
    // on the same path is impossible.
    for change in plan
        .changes
        .iter()
        .filter(|c| c.operation == "delete_document")
    {
        check_hash(&current_hashes, change)?;

        // Pass 1c.1: apply link rewrites if link_risk is attached (--rewrite-to case).
        // This runs BEFORE the delete so links can be rewritten in source docs.
        if change.link_risk.is_some() {
            if dry_run {
                // Synthesize LinkRewriteResult entries for dry-run reporting.
                if let Some(risk) = &change.link_risk {
                    for affected in risk
                        .stem_links
                        .iter()
                        .chain(risk.path_qualified_wikilinks.iter())
                        .chain(risk.markdown_links.iter())
                    {
                        report.rewritten_links.push(LinkRewriteResult {
                            file: affected.source_path.clone(),
                            from: affected.raw.clone(),
                            to: affected.rewritten.clone(),
                        });
                    }
                }
            } else {
                report
                    .rewritten_links
                    .extend(apply_link_rewrites(cwd, change)?);
            }
        }

        // Pass 1c.2: the actual file removal.
        if !dry_run {
            let result = apply_delete(cwd, change)?;
            report.deleted_documents.push(result);
        } else {
            report.deleted_documents.push(DeleteResult {
                path: change.path.clone(),
            });
        }
    }

    // Pass 1d: replace_body operations. Whole-body rewrites with hash-check
    // discipline matching Passes 1a / 1b. Sequenced after delete_document so we
    // never attempt a body-replace on a file that was just removed, and before
    // move_document so the content is settled before any rename.
    for change in plan
        .changes
        .iter()
        .filter(|c| c.operation == "replace_body")
    {
        check_hash(&current_hashes, change)?;

        if dry_run {
            if !report.changed_files.contains(&change.path) {
                report.changed_files.push(change.path.clone());
            }
            report.replaced_bodies.push(change.path.clone());
            continue;
        }

        let absolute_path = cwd.join(&change.path);
        let content =
            fs::read_to_string(&absolute_path).with_context(|| format!("read {absolute_path}"))?;
        let updated = vault_standards::apply::apply_replace_body(&content, change)?;
        if updated != content {
            fs::write(&absolute_path, &updated)
                .with_context(|| format!("write {absolute_path}"))?;
            if !report.changed_files.contains(&change.path) {
                report.changed_files.push(change.path.clone());
            }
        }
        report.replaced_bodies.push(change.path.clone());
    }

    // Collect move_document changes for passes 2 and 3.
    let move_changes: Vec<&PlannedChange> = plan
        .changes
        .iter()
        .filter(|c| c.operation == "move_document")
        .collect();

    // Pass 2: filesystem moves.
    let mut moves: Vec<MoveResult> = Vec::new();
    for change in &move_changes {
        if dry_run {
            if let Some(destination) = change.destination.as_ref() {
                moves.push(MoveResult {
                    from: change.path.clone(),
                    to: destination.clone(),
                });
            }
        } else {
            moves.push(apply_move(cwd, change)?);
        }
    }

    // Pass 3: link rewrites (only after every move succeeded).
    let mut rewrites: Vec<LinkRewriteResult> = Vec::new();
    for change in &move_changes {
        if dry_run {
            if let Some(risk) = &change.link_risk {
                for affected in risk
                    .stem_links
                    .iter()
                    .chain(risk.path_qualified_wikilinks.iter())
                    .chain(risk.markdown_links.iter())
                {
                    rewrites.push(LinkRewriteResult {
                        file: affected.source_path.clone(),
                        from: affected.raw.clone(),
                        to: affected.rewritten.clone(),
                    });
                }
            }
        } else {
            rewrites.extend(apply_link_rewrites(cwd, change)?);
        }
    }

    let warnings: Vec<RepairApplyWarning> = move_changes
        .iter()
        .flat_map(|c| {
            c.warnings.iter().map(|w| RepairApplyWarning {
                path: c.path.clone(),
                warning: w.clone(),
            })
        })
        .collect();

    report.moved_files = moves;
    // Extend (not replace): Pass 1b may have already populated rewritten_links
    // with rewrite_link results; Pass 3 appends move-induced backlink rewrites.
    report.rewritten_links.extend(rewrites);
    report.warnings = warnings;

    Ok(report)
}

pub fn with_verification(report: RepairApplyReport, findings: &[Finding]) -> RepairApplyReport {
    report.with_verification(findings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vault_standards::{
        PlannedChange, RepairPlan, RepairPlanFilters, RepairPlanSummary, SkippedSummary,
        REPAIR_PLAN_SCHEMA_VERSION,
    };

    /// Build a minimal on-disk vault with a single document and return the
    /// temp dir, the vault root as a `Utf8PathBuf`, and the `GraphIndex`.
    fn make_vault_with_doc(
        prefix: &str,
        doc_rel: &str,
        body: &str,
    ) -> (tempfile::TempDir, camino::Utf8PathBuf, GraphIndex, String) {
        let tmp = tempfile::Builder::new().prefix(prefix).tempdir().unwrap();
        let root = camino::Utf8Path::from_path(tmp.path())
            .unwrap()
            .to_path_buf();
        // Write a minimal vault config so build_index doesn't complain.
        std::fs::create_dir_all(tmp.path().join(".vault")).unwrap();
        std::fs::write(tmp.path().join(".vault/config.yaml"), "validate: {}\n").unwrap();
        std::fs::write(root.join(doc_rel), body).unwrap();
        let index = vault_graph::build_index(&root).unwrap();
        let hash = index
            .documents
            .iter()
            .find(|d| d.path == doc_rel)
            .unwrap()
            .hash
            .clone();
        (tmp, root, index, hash)
    }

    fn delete_plan(vault_root: &camino::Utf8PathBuf, doc_rel: &str, hash: &str) -> RepairPlan {
        RepairPlan {
            schema_version: REPAIR_PLAN_SCHEMA_VERSION,
            vault_root: vault_root.clone(),
            source_filters: RepairPlanFilters::default(),
            summary: RepairPlanSummary {
                findings: 0,
                planned_changes: 1,
                skipped: SkippedSummary::default(),
            },
            changes: vec![PlannedChange {
                change_id: "delete-foo".into(),
                path: doc_rel.into(),
                document_hash: hash.to_string(),
                finding_code: "operator-request".into(),
                finding_rule: None,
                repair_rule: "operator-request".into(),
                operation: "delete_document".into(),
                field: None,
                expected_old_value: None,
                new_value: None,
                destination: None,
                link_risk: None,
                warnings: Vec::new(),
                force: false,
            }],
            skipped_findings: Vec::new(),
            footnotes: Vec::new(),
        }
    }

    #[test]
    fn delete_pass_removes_file() {
        let (_tmp, root, index, hash) = make_vault_with_doc(
            "vault-cli-orch-delete-",
            "foo.md",
            "---\ntype: note\n---\n# Foo\n",
        );
        let plan = delete_plan(&root, "foo.md", &hash);

        let report = apply_repair_plan(&root, &index, &plan, /*dry_run=*/ false).unwrap();

        assert_eq!(report.deleted_documents.len(), 1);
        assert_eq!(
            report.deleted_documents[0].path,
            camino::Utf8PathBuf::from("foo.md")
        );
        assert!(!root.join("foo.md").as_std_path().exists());
    }

    #[test]
    fn delete_pass_dry_run_does_not_remove_file() {
        let (_tmp, root, index, hash) = make_vault_with_doc(
            "vault-cli-orch-delete-dry-",
            "foo.md",
            "---\ntype: note\n---\n# Foo\n",
        );
        let plan = delete_plan(&root, "foo.md", &hash);

        let report = apply_repair_plan(&root, &index, &plan, /*dry_run=*/ true).unwrap();

        // Dry run: entry is recorded but file must still exist.
        assert_eq!(report.deleted_documents.len(), 1);
        assert_eq!(
            report.deleted_documents[0].path,
            camino::Utf8PathBuf::from("foo.md")
        );
        assert!(root.join("foo.md").as_std_path().exists());
    }

    #[test]
    fn delete_pass_rejects_stale_hash() {
        let (_tmp, root, index, _hash) = make_vault_with_doc(
            "vault-cli-orch-delete-stale-",
            "foo.md",
            "---\ntype: note\n---\n# Foo\n",
        );
        // Use an intentionally wrong hash.
        let plan = delete_plan(&root, "foo.md", "definitely-wrong-hash");

        let err = apply_repair_plan(&root, &index, &plan, /*dry_run=*/ false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("stale") || msg.contains("hash"),
            "expected stale-hash error, got: {msg}"
        );
        // File must be untouched.
        assert!(root.join("foo.md").as_std_path().exists());
    }

    #[test]
    fn delete_pass_with_rewrite_to_rewrites_then_deletes() {
        use vault_standards::classify_link_risk;
        let tmp = tempfile::Builder::new()
            .prefix("vault-cli-orch-delete-rewrite-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path())
            .unwrap()
            .to_path_buf();
        std::fs::create_dir_all(tmp.path().join(".vault")).unwrap();
        std::fs::write(tmp.path().join(".vault/config.yaml"), "validate: {}\n").unwrap();
        std::fs::write(root.join("a.md"), "---\ntype: note\n---\n[[b]]\n").unwrap();
        std::fs::write(root.join("b.md"), "---\ntype: note\n---\n# B\n").unwrap();
        std::fs::write(root.join("c.md"), "---\ntype: note\n---\n# C\n").unwrap();
        let index = vault_graph::build_index(&root).unwrap();

        let b_doc = index
            .documents
            .iter()
            .find(|d| d.path.as_str() == "b.md")
            .unwrap();
        let risk = classify_link_risk(
            &camino::Utf8PathBuf::from("b.md"),
            &camino::Utf8PathBuf::from("c.md"),
            &index.documents,
            &index.files,
        );

        let plan = RepairPlan {
            schema_version: REPAIR_PLAN_SCHEMA_VERSION,
            vault_root: root.clone(),
            source_filters: RepairPlanFilters::default(),
            summary: RepairPlanSummary {
                findings: 0,
                planned_changes: 1,
                skipped: SkippedSummary::default(),
            },
            changes: vec![PlannedChange {
                change_id: "delete-b".into(),
                path: "b.md".into(),
                document_hash: b_doc.hash.clone(),
                finding_code: "operator-request".into(),
                finding_rule: None,
                repair_rule: "operator-request".into(),
                operation: "delete_document".into(),
                field: None,
                expected_old_value: None,
                new_value: None,
                destination: None,
                link_risk: Some(risk),
                warnings: Vec::new(),
                force: false,
            }],
            skipped_findings: Vec::new(),
            footnotes: Vec::new(),
        };

        let report = apply_repair_plan(&root, &index, &plan, false).unwrap();
        assert_eq!(report.deleted_documents.len(), 1);
        assert!(!root.join("b.md").as_std_path().exists());
        let a_content = std::fs::read_to_string(root.join("a.md")).unwrap();
        assert!(
            a_content.contains("[[c]]"),
            "a.md should now link to c: {a_content}"
        );
    }
}
