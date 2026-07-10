//! `norn delete` command: pre-flight validation, plan synthesis, render, dispatch.
//!
//! Plan synthesis builds a RepairPlan with one delete_document op. When
//! --rewrite-to <ALT> is provided, link_risk is attached to the
//! delete_document op (using classify_link_risk against the alt as the
//! destination); Pass 1c reads it and applies the cascade before deleting.

use std::io::Write;

pub mod route;

use crate::core::GraphIndex;
use crate::standards::{
    classify_link_risk, PlannedChange, RepairPlan, RepairPlanFilters, RepairPlanSummary,
    SkippedSummary, REPAIR_PLAN_SCHEMA_VERSION,
};
use camino::Utf8PathBuf;

// ---------------------------------------------------------------------------
// ApplyReport-based TTY renderer for delete
// ---------------------------------------------------------------------------

/// Render a human-readable TTY summary for a delete operation.
///
/// `doc` is the vault-relative path of the deleted document.
/// `incoming_total` is the count of backlinks (from preflight).
/// `incoming_files` is the list of source file paths that hold backlinks.
/// `rewrite_to` is the resolved rewrite target path (if `--rewrite-to` was used).
/// `rewrite_total` is the number of links rewritten (from link_risk; 0 if no rewrite).
/// `applied` is `true` when the operation was executed, `false` for dry-run/preview.
pub fn render_delete_apply_tty<W: Write>(
    out: &mut W,
    doc: &str,
    incoming_total: usize,
    incoming_files: &[camino::Utf8PathBuf],
    rewrite_to: Option<&str>,
    rewrite_total: usize,
    applied: bool,
) -> std::io::Result<()> {
    macro_rules! pl {
        ($n:expr, $s:literal, $p:literal) => {
            if $n == 1 {
                $s
            } else {
                $p
            }
        };
    }

    if applied {
        match rewrite_to {
            Some(alt) => {
                writeln!(out, "✓ deleted {doc} (incoming links redirected to {alt})")?;
                writeln!(
                    out,
                    "✓ rewrote {} {} across {} {}",
                    rewrite_total,
                    pl!(rewrite_total, "backlink", "backlinks"),
                    incoming_files.len(),
                    pl!(incoming_files.len(), "file", "files"),
                )?;
            }
            None => {
                writeln!(out, "✓ deleted {doc}")?;
                if incoming_total > 0 {
                    writeln!(
                        out,
                        "⚠ {} {} now broken (surface via norn validate)",
                        incoming_total,
                        pl!(incoming_total, "link", "links"),
                    )?;
                }
            }
        }
    } else {
        match rewrite_to {
            Some(alt) => {
                writeln!(
                    out,
                    "norn delete {doc} → redirects {} incoming {} to {alt}",
                    incoming_total,
                    pl!(incoming_total, "link", "links"),
                )?;
                writeln!(
                    out,
                    "  {} {} to rewrite across {} {}",
                    rewrite_total,
                    pl!(rewrite_total, "backlink", "backlinks"),
                    incoming_files.len(),
                    pl!(incoming_files.len(), "file", "files"),
                )?;
            }
            None => {
                writeln!(out, "norn delete {doc}")?;
                if incoming_total > 0 {
                    writeln!(
                        out,
                        "  ⚠ {} incoming {} will break across {} {}:",
                        incoming_total,
                        pl!(incoming_total, "link", "links"),
                        incoming_files.len(),
                        pl!(incoming_files.len(), "file", "files"),
                    )?;
                    for file in incoming_files {
                        writeln!(out, "      {file}")?;
                    }
                    writeln!(
                        out,
                        "  (broken links will surface as link-target-missing findings in `norn validate`)"
                    )?;
                }
            }
        }
    }
    Ok(())
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

impl DeletePreflightError {
    /// The stable, machine-branchable kebab code for this preflight refusal
    /// (NRN-229), so an MCP `vault.delete` consumer / a `--format json` CLI caller
    /// branches on the code rather than string-matching the prose. `Display` is
    /// unchanged (byte-identical CLI/stderr output); the code rides alongside.
    ///
    /// Reuses `set`'s `target-not-found` / `target-ambiguous` for the primary
    /// target (identical semantic: the DOC argument does not resolve, or resolves
    /// to more than one indexed document — NRN-235, one-semantic-one-code). The
    /// `--rewrite-to` alternate target gets its own `rewrite-to-*` codes because
    /// the remediation differs (fix the `--rewrite-to` argument, not the target),
    /// and `backlinks-present` is the delete-specific policy refusal.
    ///
    /// NOTE: distinct from the applier's apply-time `delete-source-missing`
    /// lifecycle code — that fires when the file is missing on the FILESYSTEM at
    /// apply time; `target-not-found` fires earlier, when the argument does not
    /// resolve against the graph index.
    pub fn code(&self) -> &'static str {
        match self {
            DeletePreflightError::DocMissing(_) => "target-not-found",
            DeletePreflightError::DocAmbiguous { .. } => "target-ambiguous",
            DeletePreflightError::IncomingLinksRefused { .. } => "backlinks-present",
            DeletePreflightError::RewriteToMissing(_) => "rewrite-to-not-found",
            DeletePreflightError::RewriteToSelf => "rewrite-to-self",
            DeletePreflightError::RewriteToAmbiguous { .. } => "rewrite-to-ambiguous",
        }
    }
}

pub(crate) struct PreflightConfig<'a> {
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

pub(crate) fn preflight_and_plan(
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
        parents: false,
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
            .prefix("norn-delete-preflight-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path())
            .unwrap()
            .to_path_buf();

        // Minimal vault config required by build_index.
        std::fs::create_dir_all(tmp.path().join(".norn")).unwrap();
        std::fs::write(tmp.path().join(".norn/config.yaml"), "validate: {}\n").unwrap();

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

    /// NRN-229: every `DeletePreflightError` variant carries a stable kebab
    /// `.code()`. The primary target REUSES `set`'s `target-*` codes; the
    /// `--rewrite-to` alternate and the backlink-policy refusal get their own.
    #[test]
    fn delete_preflight_error_codes_are_stable() {
        let cases: Vec<(DeletePreflightError, &str)> = vec![
            (
                DeletePreflightError::DocMissing("x".into()),
                "target-not-found",
            ),
            (
                DeletePreflightError::DocAmbiguous {
                    stem: "x".into(),
                    candidates: vec![],
                },
                "target-ambiguous",
            ),
            (
                DeletePreflightError::IncomingLinksRefused { count: 3 },
                "backlinks-present",
            ),
            (
                DeletePreflightError::RewriteToMissing("y".into()),
                "rewrite-to-not-found",
            ),
            (DeletePreflightError::RewriteToSelf, "rewrite-to-self"),
            (
                DeletePreflightError::RewriteToAmbiguous {
                    stem: "y".into(),
                    candidates: vec![],
                },
                "rewrite-to-ambiguous",
            ),
        ];
        for (err, code) in cases {
            assert_eq!(err.code(), code, "wrong code for {err:?}");
            assert!(
                code.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "code {code:?} is not kebab-case"
            );
        }
    }
}
