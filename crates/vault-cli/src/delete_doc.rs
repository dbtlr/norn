//! `vault delete` command: pre-flight validation, plan synthesis, render, dispatch.
//!
//! Plan synthesis builds a RepairPlan with one delete_document op. When
//! --rewrite-to <ALT> is provided, link_risk is attached to the
//! delete_document op (using classify_link_risk against the alt as the
//! destination); Pass 1c reads it and applies the cascade before deleting.

use std::io::Write;

use crate::standards::{
    classify_link_risk, PlannedChange, RepairPlan, RepairPlanFilters, RepairPlanSummary,
    SkippedSummary, REPAIR_PLAN_SCHEMA_VERSION,
};
use camino::Utf8PathBuf;
use serde::Serialize;
use vault_core::GraphIndex;

use crate::mutation_report::{LinkFile, LinkSummary};

// ---------------------------------------------------------------------------
// Report types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct DeleteReport {
    /// Independent of repair_plan_schema_version; bumps when the DeleteReport shape changes.
    pub schema_version: u32,
    pub operation: String,
    pub target: Utf8PathBuf,
    pub incoming_links: LinkSummary,
    pub rewrite_to: Option<Utf8PathBuf>,
    pub link_rewrites: Option<LinkSummary>,
    pub applied: bool,
    pub warnings: Vec<DeleteWarning>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeleteWarning {
    CodeFenceSuspected { path: Utf8PathBuf, count: usize },
}

// ---------------------------------------------------------------------------
// Renderers
// ---------------------------------------------------------------------------

pub fn render_records<W: Write>(out: &mut W, report: &DeleteReport) -> std::io::Result<()> {
    let plural = |n: usize, s: &str, p: &str| -> String {
        if n == 1 {
            s.to_string()
        } else {
            p.to_string()
        }
    };

    if report.applied {
        match (&report.rewrite_to, &report.link_rewrites) {
            (Some(alt), Some(rewrites)) => {
                writeln!(
                    out,
                    "✓ deleted {} (incoming links redirected to {alt})",
                    report.target
                )?;
                writeln!(
                    out,
                    "✓ rewrote {} {} across {} {}",
                    rewrites.total,
                    plural(rewrites.total, "backlink", "backlinks"),
                    rewrites.files.len(),
                    plural(rewrites.files.len(), "file", "files"),
                )?;
            }
            _ => {
                writeln!(out, "✓ deleted {}", report.target)?;
                if report.incoming_links.total > 0 {
                    writeln!(
                        out,
                        "⚠ {} {} now broken (surface via vault validate)",
                        report.incoming_links.total,
                        plural(report.incoming_links.total, "link", "links"),
                    )?;
                }
            }
        }
    } else {
        match (&report.rewrite_to, &report.link_rewrites) {
            (Some(alt), Some(rewrites)) => {
                writeln!(
                    out,
                    "vault delete {} → redirects {} incoming {} to {alt}",
                    report.target,
                    report.incoming_links.total,
                    plural(report.incoming_links.total, "link", "links"),
                )?;
                writeln!(
                    out,
                    "  {} {} to rewrite across {} {}",
                    rewrites.total,
                    plural(rewrites.total, "backlink", "backlinks"),
                    rewrites.files.len(),
                    plural(rewrites.files.len(), "file", "files"),
                )?;
            }
            _ => {
                writeln!(out, "vault delete {}", report.target)?;
                if report.incoming_links.total > 0 {
                    writeln!(
                        out,
                        "  ⚠ {} incoming {} will break across {} {}:",
                        report.incoming_links.total,
                        plural(report.incoming_links.total, "link", "links"),
                        report.incoming_links.files.len(),
                        plural(report.incoming_links.files.len(), "file", "files"),
                    )?;
                    for file in &report.incoming_links.files {
                        writeln!(out, "      {}", file.path)?;
                    }
                    writeln!(
                        out,
                        "  (broken links will surface as link-target-missing findings in `vault validate`)"
                    )?;
                }
            }
        }
    }
    Ok(())
}

pub fn render_json<W: Write>(out: &mut W, report: &DeleteReport) -> anyhow::Result<()> {
    serde_json::to_writer_pretty(&mut *out, report)?;
    writeln!(out)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// build_report
// ---------------------------------------------------------------------------

/// Build a `DeleteReport` from a `RepairPlan` and the current graph index.
///
/// `rewrite_to` is the resolved vault-relative path of the `--rewrite-to` target
/// (if provided). `applied` should be `false` for the preview and `true` after
/// `apply_repair_plan` has completed.
pub fn build_report(
    plan: &RepairPlan,
    index: &GraphIndex,
    rewrite_to: Option<&Utf8PathBuf>,
    applied: bool,
) -> DeleteReport {
    use std::collections::BTreeMap;

    let delete_op = plan
        .changes
        .iter()
        .find(|c| c.operation == "delete_document")
        .expect("plan must contain delete_document op");

    // Incoming links: enumerate via target::backlinks (the index source-of-truth).
    let bl = crate::target::backlinks(index, &delete_op.path);
    let mut incoming_counts: BTreeMap<Utf8PathBuf, usize> = BTreeMap::new();
    for link in &bl {
        *incoming_counts.entry(link.source_path.clone()).or_insert(0) += 1;
    }
    let incoming_links = LinkSummary {
        total: bl.len(),
        files: incoming_counts
            .into_iter()
            .map(|(path, count)| LinkFile { path, count })
            .collect(),
    };

    // link_rewrites: derived from the delete_op's link_risk when --rewrite-to was used.
    let link_rewrites = delete_op.link_risk.as_ref().map(|risk| {
        let mut counts: BTreeMap<Utf8PathBuf, usize> = BTreeMap::new();
        let mut total = 0;
        for affected in risk
            .stem_links
            .iter()
            .chain(risk.path_qualified_wikilinks.iter())
            .chain(risk.markdown_links.iter())
        {
            *counts.entry(affected.source_path.clone()).or_insert(0) += 1;
            total += 1;
        }
        LinkSummary {
            total,
            files: counts
                .into_iter()
                .map(|(path, count)| LinkFile { path, count })
                .collect(),
        }
    });

    DeleteReport {
        schema_version: 1,
        operation: "delete".into(),
        target: delete_op.path.clone(),
        incoming_links,
        rewrite_to: rewrite_to.cloned(),
        link_rewrites,
        applied,
        warnings: Vec::new(),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DeletePreflightError {
    #[error("document does not exist: {0}")]
    DocMissing(String),
    #[error("document resolves ambiguously by stem: {stem} → {candidates:?}")]
    DocAmbiguous {
        stem: String,
        candidates: Vec<Utf8PathBuf>,
    },
    #[error("document has {count} incoming link(s); pass --allow-broken-links to accept, or --rewrite-to <ALT_DOC> to redirect")]
    IncomingLinksRefused { count: usize },
    #[error("rewrite-to target does not exist: {0}")]
    RewriteToMissing(String),
    #[error("rewrite-to target resolves to the same document as target")]
    RewriteToSelf,
    #[error("rewrite-to target resolves ambiguously by stem: {stem} → {candidates:?}")]
    RewriteToAmbiguous {
        stem: String,
        candidates: Vec<Utf8PathBuf>,
    },
}

pub struct PreflightConfig<'a> {
    pub doc: &'a str,
    pub allow_broken_links: bool,
    pub rewrite_to: Option<&'a str>,
    pub vault_root: &'a Utf8PathBuf,
    pub index: &'a GraphIndex,
}

/// The outcome of a successful `preflight_and_plan` call.
#[derive(Debug)]
pub struct PreflightOutcome {
    pub plan: RepairPlan,
    /// The resolved vault-relative path of the `--rewrite-to` target, if provided.
    pub resolved_rewrite_to: Option<Utf8PathBuf>,
}

pub fn preflight_and_plan(
    cfg: PreflightConfig<'_>,
) -> Result<PreflightOutcome, DeletePreflightError> {
    use crate::target::{backlinks, resolve_target_path};

    // Resolve doc path.
    let doc_rel = resolve_target_path(cfg.index, cfg.doc).map_err(|e| {
        let msg = format!("{e}");
        if msg.contains("ambiguous") {
            DeletePreflightError::DocAmbiguous {
                stem: cfg.doc.to_string(),
                candidates: Vec::new(),
            }
        } else {
            DeletePreflightError::DocMissing(cfg.doc.to_string())
        }
    })?;

    let doc = cfg
        .index
        .documents
        .iter()
        .find(|d| d.path == doc_rel)
        .ok_or_else(|| DeletePreflightError::DocMissing(cfg.doc.to_string()))?;

    // Count incoming links.
    let incoming = backlinks(cfg.index, &doc_rel);

    // Resolve rewrite-to target if provided.
    let rewrite_to_rel = if let Some(alt) = cfg.rewrite_to {
        let alt_rel = resolve_target_path(cfg.index, alt).map_err(|e| {
            let msg = format!("{e}");
            if msg.contains("ambiguous") {
                DeletePreflightError::RewriteToAmbiguous {
                    stem: alt.to_string(),
                    candidates: Vec::new(),
                }
            } else {
                DeletePreflightError::RewriteToMissing(alt.to_string())
            }
        })?;
        if alt_rel == doc_rel {
            return Err(DeletePreflightError::RewriteToSelf);
        }
        Some(alt_rel)
    } else {
        None
    };

    // If incoming links exist and neither flag is set, refuse.
    if !incoming.is_empty() && rewrite_to_rel.is_none() && !cfg.allow_broken_links {
        return Err(DeletePreflightError::IncomingLinksRefused {
            count: incoming.len(),
        });
    }

    // Build the delete_document change. Attach link_risk when --rewrite-to.
    let link_risk = rewrite_to_rel
        .as_ref()
        .map(|alt| classify_link_risk(&doc_rel, alt, &cfg.index.documents, &cfg.index.files));

    let delete_change = PlannedChange {
        change_id: format!("delete-{}", doc_rel),
        path: doc_rel.clone(),
        document_hash: doc.hash.clone(),
        finding_code: "operator-request".into(),
        finding_rule: None,
        repair_rule: "operator-request".into(),
        operation: "delete_document".into(),
        field: None,
        expected_old_value: None,
        new_value: None,
        destination: None,
        link_risk,
        warnings: Vec::new(),
        force: false,
    };

    Ok(PreflightOutcome {
        plan: RepairPlan {
            schema_version: REPAIR_PLAN_SCHEMA_VERSION,
            vault_root: cfg.vault_root.clone(),
            source_filters: RepairPlanFilters::default(),
            summary: RepairPlanSummary {
                findings: 0,
                planned_changes: 1,
                skipped: SkippedSummary::default(),
            },
            changes: vec![delete_change],
            skipped_findings: Vec::new(),
            footnotes: Vec::new(),
        },
        resolved_rewrite_to: rewrite_to_rel,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_vault() -> (tempfile::TempDir, Utf8PathBuf, GraphIndex) {
        let tmp = tempfile::Builder::new()
            .prefix("vault-cli-delete-preflight-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path())
            .unwrap()
            .to_path_buf();

        // Minimal vault config required by build_index.
        std::fs::create_dir_all(tmp.path().join(".vault")).unwrap();
        std::fs::write(tmp.path().join(".vault/config.yaml"), "validate: {}\n").unwrap();

        std::fs::write(root.join("a.md"), "---\ntype: note\n---\n# A\n[[b]]\n").unwrap();
        std::fs::write(root.join("b.md"), "---\ntype: note\n---\n# B\n").unwrap();
        std::fs::write(root.join("c.md"), "---\ntype: note\n---\n# C\n").unwrap();
        let index = crate::graph::build_index(&root).unwrap();
        (tmp, root, index)
    }

    #[test]
    fn delete_doc_missing_errors() {
        let (_tmp, root, index) = fixture_vault();
        let result = preflight_and_plan(PreflightConfig {
            doc: "nope.md",
            allow_broken_links: false,
            rewrite_to: None,
            vault_root: &root,
            index: &index,
        });
        assert!(matches!(result, Err(DeletePreflightError::DocMissing(_))));
    }

    #[test]
    fn delete_refused_when_incoming_links_no_flag() {
        let (_tmp, root, index) = fixture_vault();
        let result = preflight_and_plan(PreflightConfig {
            doc: "b.md",
            allow_broken_links: false,
            rewrite_to: None,
            vault_root: &root,
            index: &index,
        });
        match result {
            Err(DeletePreflightError::IncomingLinksRefused { count }) => assert_eq!(count, 1),
            other => panic!("expected IncomingLinksRefused, got {other:?}"),
        }
    }

    #[test]
    fn delete_with_allow_broken_links_succeeds() {
        let (_tmp, root, index) = fixture_vault();
        let outcome = preflight_and_plan(PreflightConfig {
            doc: "b.md",
            allow_broken_links: true,
            rewrite_to: None,
            vault_root: &root,
            index: &index,
        })
        .unwrap();
        let plan = &outcome.plan;
        assert_eq!(plan.changes.len(), 1);
        assert_eq!(plan.changes[0].operation, "delete_document");
        assert!(plan.changes[0].link_risk.is_none());
        assert!(outcome.resolved_rewrite_to.is_none());
    }

    #[test]
    fn delete_with_rewrite_to_attaches_link_risk() {
        let (_tmp, root, index) = fixture_vault();
        let outcome = preflight_and_plan(PreflightConfig {
            doc: "b.md",
            allow_broken_links: false,
            rewrite_to: Some("c.md"),
            vault_root: &root,
            index: &index,
        })
        .unwrap();
        let plan = &outcome.plan;
        assert_eq!(plan.changes.len(), 1);
        assert_eq!(plan.changes[0].operation, "delete_document");
        let risk = plan.changes[0]
            .link_risk
            .as_ref()
            .expect("link_risk should be Some");
        let total =
            risk.stem_links.len() + risk.path_qualified_wikilinks.len() + risk.markdown_links.len();
        assert!(
            total >= 1,
            "a.md links to b — at least one affected record expected"
        );
        assert_eq!(
            outcome.resolved_rewrite_to.as_deref(),
            Some(camino::Utf8Path::new("c.md"))
        );
    }

    #[test]
    fn delete_rewrite_to_self_errors() {
        let (_tmp, root, index) = fixture_vault();
        let result = preflight_and_plan(PreflightConfig {
            doc: "b.md",
            allow_broken_links: false,
            rewrite_to: Some("b.md"),
            vault_root: &root,
            index: &index,
        });
        assert!(matches!(result, Err(DeletePreflightError::RewriteToSelf)));
    }

    // ---------------------------------------------------------------------------
    // build_report tests
    // ---------------------------------------------------------------------------

    #[test]
    fn build_report_counts_incoming_links() {
        let (_tmp, root, index) = fixture_vault();
        let outcome = preflight_and_plan(PreflightConfig {
            doc: "b.md",
            allow_broken_links: true,
            rewrite_to: None,
            vault_root: &root,
            index: &index,
        })
        .unwrap();
        let report = build_report(&outcome.plan, &index, None, false);
        assert_eq!(report.operation, "delete");
        assert_eq!(report.target.as_str(), "b.md");
        assert_eq!(report.incoming_links.total, 1);
        assert_eq!(report.incoming_links.files.len(), 1);
        assert_eq!(report.incoming_links.files[0].path.as_str(), "a.md");
        assert!(report.link_rewrites.is_none());
        assert!(report.rewrite_to.is_none());
        assert!(!report.applied);
    }

    #[test]
    fn build_report_with_rewrite_to_has_link_rewrites() {
        let (_tmp, root, index) = fixture_vault();
        let outcome = preflight_and_plan(PreflightConfig {
            doc: "b.md",
            allow_broken_links: false,
            rewrite_to: Some("c.md"),
            vault_root: &root,
            index: &index,
        })
        .unwrap();
        let rewrite_to = outcome.resolved_rewrite_to.clone();
        let report = build_report(&outcome.plan, &index, rewrite_to.as_ref(), false);
        assert_eq!(
            report.rewrite_to.as_deref(),
            Some(camino::Utf8Path::new("c.md"))
        );
        let rewrites = report.link_rewrites.expect("link_rewrites should be Some");
        assert!(rewrites.total >= 1);
    }

    // ---------------------------------------------------------------------------
    // Renderer tests
    // ---------------------------------------------------------------------------

    #[test]
    fn render_records_preview_with_no_incoming_links() {
        let report = DeleteReport {
            schema_version: 1,
            operation: "delete".into(),
            target: "leaf.md".into(),
            incoming_links: LinkSummary {
                total: 0,
                files: Vec::new(),
            },
            rewrite_to: None,
            link_rewrites: None,
            applied: false,
            warnings: Vec::new(),
        };
        let mut buf = Vec::new();
        render_records(&mut buf, &report).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("vault delete leaf.md"), "unexpected: {out}");
        // No incoming link warning expected.
        assert!(!out.contains("will break"), "unexpected: {out}");
    }

    #[test]
    fn render_records_preview_with_incoming_links_warns() {
        let report = DeleteReport {
            schema_version: 1,
            operation: "delete".into(),
            target: "b.md".into(),
            incoming_links: LinkSummary {
                total: 1,
                files: vec![LinkFile {
                    path: "a.md".into(),
                    count: 1,
                }],
            },
            rewrite_to: None,
            link_rewrites: None,
            applied: false,
            warnings: Vec::new(),
        };
        let mut buf = Vec::new();
        render_records(&mut buf, &report).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("vault delete b.md"), "unexpected: {out}");
        assert!(
            out.contains("1 incoming link will break"),
            "unexpected: {out}"
        );
        assert!(out.contains("a.md"), "unexpected: {out}");
    }

    #[test]
    fn render_records_applied_emits_checkmark() {
        let report = DeleteReport {
            schema_version: 1,
            operation: "delete".into(),
            target: "leaf.md".into(),
            incoming_links: LinkSummary {
                total: 0,
                files: Vec::new(),
            },
            rewrite_to: None,
            link_rewrites: None,
            applied: true,
            warnings: Vec::new(),
        };
        let mut buf = Vec::new();
        render_records(&mut buf, &report).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("✓ deleted leaf.md"), "unexpected: {out}");
    }

    #[test]
    fn render_records_applied_with_rewrite_to() {
        let report = DeleteReport {
            schema_version: 1,
            operation: "delete".into(),
            target: "b.md".into(),
            incoming_links: LinkSummary {
                total: 1,
                files: vec![LinkFile {
                    path: "a.md".into(),
                    count: 1,
                }],
            },
            rewrite_to: Some("c.md".into()),
            link_rewrites: Some(LinkSummary {
                total: 1,
                files: vec![LinkFile {
                    path: "a.md".into(),
                    count: 1,
                }],
            }),
            applied: true,
            warnings: Vec::new(),
        };
        let mut buf = Vec::new();
        render_records(&mut buf, &report).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("✓ deleted b.md"), "unexpected: {out}");
        assert!(out.contains("redirected to c.md"), "unexpected: {out}");
        assert!(out.contains("rewrote 1 backlink"), "unexpected: {out}");
    }

    #[test]
    fn render_json_emits_valid_envelope() {
        let report = DeleteReport {
            schema_version: 1,
            operation: "delete".into(),
            target: "b.md".into(),
            incoming_links: LinkSummary {
                total: 1,
                files: vec![LinkFile {
                    path: "a.md".into(),
                    count: 1,
                }],
            },
            rewrite_to: None,
            link_rewrites: None,
            applied: false,
            warnings: Vec::new(),
        };
        let mut buf = Vec::new();
        render_json(&mut buf, &report).unwrap();
        let json = String::from_utf8(buf).unwrap();
        assert!(json.contains(r#""operation": "delete""#));
        assert!(json.contains(r#""target": "b.md""#));
        assert!(json.contains(r#""applied": false"#));
        assert!(json.contains(r#""rewrite_to": null"#));
    }
}
