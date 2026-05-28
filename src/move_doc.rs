//! `norn move` command: pre-flight validation, plan synthesis, render, dispatch.
//!
//! Plan synthesis builds a RepairPlan with a single move_document op. The
//! link_risk field on that op carries all affected backlinks; the existing
//! apply orchestrator's Pass 3 reads link_risk from move_document and handles
//! the cascade — no separate rewrite_link ops are emitted.

use std::io::Write;

use crate::core::GraphIndex;
use crate::standards::{
    classify_link_risk, PlannedChange, RepairPlan, RepairPlanFilters, RepairPlanSummary,
    SkippedSummary, REPAIR_PLAN_SCHEMA_VERSION,
};
use camino::Utf8PathBuf;
use serde::Serialize;

use crate::mutation_report::{LinkFile, LinkSummary};

// ---------------------------------------------------------------------------
// Report types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct MoveReport {
    /// Independent of repair_plan_schema_version; bumps when the MoveReport shape changes.
    pub schema_version: u32,
    pub operation: String,
    pub source: Utf8PathBuf,
    pub destination: Utf8PathBuf,
    pub link_rewrites: LinkSummary,
    pub applied: bool,
    pub warnings: Vec<MoveWarning>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MoveWarning {
    CodeFenceSuspected {
        path: Utf8PathBuf,
        count: usize,
    },
    OutgoingRelativePathLink {
        source: Utf8PathBuf,
        raw: String,
    },
    StemCollision {
        stem: String,
        existing_paths: Vec<Utf8PathBuf>,
    },
}

// ---------------------------------------------------------------------------
// Renderers
// ---------------------------------------------------------------------------

pub fn render_records<W: Write>(out: &mut W, report: &MoveReport) -> std::io::Result<()> {
    if report.applied {
        writeln!(out, "✓ moved {} → {}", report.source, report.destination)?;
        if report.link_rewrites.total > 0 {
            writeln!(
                out,
                "✓ rewrote {} backlink{} across {} file{}",
                report.link_rewrites.total,
                if report.link_rewrites.total == 1 {
                    ""
                } else {
                    "s"
                },
                report.link_rewrites.files.len(),
                if report.link_rewrites.files.len() == 1 {
                    ""
                } else {
                    "s"
                },
            )?;
        }
    } else {
        writeln!(out, "norn move {} → {}", report.source, report.destination)?;
        if report.link_rewrites.total > 0 {
            writeln!(
                out,
                "  {} backlink{} to rewrite across {} file{}",
                report.link_rewrites.total,
                if report.link_rewrites.total == 1 {
                    ""
                } else {
                    "s"
                },
                report.link_rewrites.files.len(),
                if report.link_rewrites.files.len() == 1 {
                    ""
                } else {
                    "s"
                },
            )?;
        } else {
            writeln!(out, "  no backlinks to rewrite")?;
        }
    }

    for warning in &report.warnings {
        match warning {
            MoveWarning::CodeFenceSuspected { path, count } => {
                writeln!(
                    out,
                    "  ⚠ code fence in {path} contains {count} stem reference{}; rewrite may touch fenced code",
                    if *count == 1 { "" } else { "s" }
                )?;
            }
            MoveWarning::OutgoingRelativePathLink { source, raw } => {
                writeln!(
                    out,
                    "  ⚠ {source} contains relative-path link {raw}; will break after move"
                )?;
            }
            MoveWarning::StemCollision {
                stem,
                existing_paths,
            } => {
                writeln!(
                    out,
                    "  ⚠ stem '{stem}' already exists at: {}",
                    existing_paths
                        .iter()
                        .map(|p| p.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )?;
            }
        }
    }
    Ok(())
}

pub fn render_json<W: Write>(out: &mut W, report: &MoveReport) -> anyhow::Result<()> {
    serde_json::to_writer_pretty(&mut *out, report)?;
    writeln!(out)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// ApplyReport-based TTY renderer for single-file moves
// ---------------------------------------------------------------------------

/// Render a human-readable TTY summary for a single-file move.
///
/// `src` and `dst` are vault-relative paths. `link_total` and `link_files` are
/// the counts derived from `link_risk` before the apply (from `classify_link_risk`).
/// `applied` is `true` when the move was executed, `false` for dry-run/preview.
pub fn render_move_apply_tty<W: Write>(
    out: &mut W,
    src: &str,
    dst: &str,
    link_total: usize,
    link_files: usize,
    applied: bool,
) -> std::io::Result<()> {
    if applied {
        writeln!(out, "✓ moved {src} → {dst}")?;
        if link_total > 0 {
            writeln!(
                out,
                "✓ rewrote {} backlink{} across {} file{}",
                link_total,
                if link_total == 1 { "" } else { "s" },
                link_files,
                if link_files == 1 { "" } else { "s" },
            )?;
        }
    } else {
        writeln!(out, "norn move {src} → {dst}")?;
        if link_total > 0 {
            writeln!(
                out,
                "  {} backlink{} to rewrite across {} file{}",
                link_total,
                if link_total == 1 { "" } else { "s" },
                link_files,
                if link_files == 1 { "" } else { "s" },
            )?;
        } else {
            writeln!(out, "  no backlinks to rewrite")?;
        }
    }
    Ok(())
}

/// Render a human-readable TTY summary for a folder move (`ApplyReport`).
pub fn render_folder_apply_tty<W: Write>(
    out: &mut W,
    report: &crate::apply_report::ApplyReport,
    dry_run: bool,
) -> std::io::Result<()> {
    let status_label = if dry_run { "dry-run" } else { "applied" };
    writeln!(out, "move-folder {status_label}")?;
    writeln!(
        out,
        "  applied: {}  skipped: {}  failed: {}",
        report.applied, report.skipped, report.failed
    )?;
    for op in &report.operations {
        let status = format!("{:?}", op.status).to_lowercase();
        writeln!(out, "  [{status}] {}", op.summary)?;
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum MovePreflightError {
    #[error("source does not exist: {0}")]
    SourceMissing(String),
    #[error("source resolves ambiguously by stem: {stem} → {candidates:?}")]
    SourceAmbiguous {
        stem: String,
        candidates: Vec<Utf8PathBuf>,
    },
    #[error("destination already exists: {0} (pass --force to overwrite)")]
    DestinationExists(Utf8PathBuf),
    #[error("destination parent directory does not exist: {0}")]
    DestinationParentMissing(Utf8PathBuf),
    #[error("source and destination resolve to the same canonical path: {0}")]
    SamePath(Utf8PathBuf),
}

pub(crate) struct PreflightConfig<'a> {
    pub src: &'a str,
    pub dst: &'a str,
    pub force: bool,
    pub no_link_rewrite: bool,
    pub vault_root: &'a Utf8PathBuf,
    pub index: &'a GraphIndex,
}

pub(crate) fn preflight_and_plan(
    cfg: PreflightConfig<'_>,
) -> Result<RepairPlan, MovePreflightError> {
    // --- Pre-flight: resolve source ---

    // Attempt exact path match first, then stem match.
    let src_rel = resolve_src(cfg.index, cfg.src)?;

    // --- Pre-flight: resolve destination (vault-relative) ---

    let dst_rel = Utf8PathBuf::from(cfg.dst);

    // Check for same-path (no-op) — must run before existence check so that
    // `--force` cannot accidentally silence this.
    {
        let src_abs = cfg.vault_root.join(&src_rel);
        let dst_abs = cfg.vault_root.join(&dst_rel);
        // Canonicalise where possible; fall back to raw comparison.
        let src_canon = src_abs
            .as_std_path()
            .canonicalize()
            .ok()
            .and_then(|p| camino::Utf8PathBuf::from_path_buf(p).ok());
        let dst_canon = dst_abs
            .as_std_path()
            .canonicalize()
            .ok()
            .and_then(|p| camino::Utf8PathBuf::from_path_buf(p).ok());

        let same = match (src_canon, dst_canon) {
            // Both exist and canonicalized: exact FS comparison (handles
            // case-insensitive filesystems like macOS default APFS).
            (Some(s), Some(d)) => s == d,
            // dst doesn't exist yet: compare the raw relative paths.
            _ => src_rel == dst_rel,
        };
        if same {
            return Err(MovePreflightError::SamePath(src_rel));
        }
    }

    // Destination parent must exist.
    if let Some(parent) = dst_rel.parent() {
        if !parent.as_str().is_empty() {
            let parent_abs = cfg.vault_root.join(parent);
            if !parent_abs.as_std_path().exists() {
                return Err(MovePreflightError::DestinationParentMissing(
                    parent.to_path_buf(),
                ));
            }
        }
    }

    // Destination must not already exist unless --force.
    let dst_abs = cfg.vault_root.join(&dst_rel);
    if dst_abs.as_std_path().exists() && !cfg.force {
        return Err(MovePreflightError::DestinationExists(dst_rel));
    }

    // --- Plan synthesis ---

    // Look up document hash from the index.
    let src_hash = cfg
        .index
        .documents
        .iter()
        .find(|d| d.path == src_rel)
        .map(|d| d.hash.clone())
        .unwrap_or_default();

    // Compute link risk when cascade rewrites are enabled. The full LinkRisk
    // (across all three vecs) lives on the move_document op; Pass 3 in the
    // apply orchestrator reads it directly — no separate rewrite_link ops.
    let link_risk = if cfg.no_link_rewrite {
        None
    } else {
        Some(classify_link_risk(
            &src_rel,
            &dst_rel,
            &cfg.index.documents,
            &cfg.index.files,
        ))
    };

    let move_change = PlannedChange {
        change_id: format!("move-{}", src_rel),
        path: src_rel.clone(),
        document_hash: src_hash,
        finding_code: "operator-request".into(),
        finding_rule: None,
        repair_rule: "operator-request".into(),
        operation: "move_document".into(),
        field: None,
        expected_old_value: None,
        new_value: None,
        destination: Some(dst_rel.clone()),
        link_risk,
        warnings: Vec::new(),
        force: cfg.force,
        parents: false,
    };

    let changes = vec![move_change];

    Ok(RepairPlan {
        schema_version: REPAIR_PLAN_SCHEMA_VERSION,
        vault_root: cfg.vault_root.clone(),
        source_filters: RepairPlanFilters::default(),
        summary: RepairPlanSummary {
            findings: 0,
            planned_changes: 1,
            skipped: SkippedSummary::default(),
        },
        changes,
        skipped_findings: Vec::new(),
        footnotes: Vec::new(),
    })
}

/// Collect preflight warnings for a move operation.
///
/// Inspects the plan and the current graph index to surface three warning kinds:
/// - `StemCollision`: the destination stem already exists elsewhere in the vault.
/// - `CodeFenceSuspected`: an affected backlink file contains the src stem inside a fenced code block.
/// - `OutgoingRelativePathLink`: the source document contains a relative-path Markdown link that will
///   break after the file moves to a different directory.
#[allow(dead_code)] // used only in unit tests; production path removed in Task 14; deleted in Task 20
pub(crate) fn collect_warnings(
    plan: &RepairPlan,
    index: &GraphIndex,
    vault_root: &Utf8PathBuf,
) -> Vec<MoveWarning> {
    use crate::standards::detect_stem_collision;

    let mut warnings = Vec::new();

    let move_op = plan
        .changes
        .iter()
        .find(|c| c.operation == "move_document")
        .expect("plan must contain a move_document op");
    let src_rel = &move_op.path;
    let dst_rel = move_op
        .destination
        .as_ref()
        .expect("move_document.destination must be set");

    // 1. Stem collision: destination stem already exists elsewhere.
    if let Some(warn) = detect_stem_collision(src_rel, dst_rel, &index.documents) {
        match warn {
            crate::standards::PlanWarning::StemCollisionAfterMove {
                new_stem,
                collides_with,
                ..
            } => {
                warnings.push(MoveWarning::StemCollision {
                    stem: new_stem,
                    existing_paths: collides_with,
                });
            }
        }
    }

    // 2. Code-fence-suspected: for each affected backlink source, scan for the
    //    src stem inside fenced code blocks.
    if let Some(risk) = &move_op.link_risk {
        use std::collections::BTreeSet;
        let mut affected_files: BTreeSet<Utf8PathBuf> = BTreeSet::new();
        for affected in risk
            .stem_links
            .iter()
            .chain(risk.path_qualified_wikilinks.iter())
            .chain(risk.markdown_links.iter())
        {
            affected_files.insert(affected.source_path.clone());
        }
        let stem = src_rel.file_stem().unwrap_or("");
        for affected_path in affected_files {
            let abs = vault_root.join(&affected_path);
            if let Ok(body) = std::fs::read_to_string(abs.as_std_path()) {
                let count = count_stem_in_code_fences(&body, stem);
                if count > 0 {
                    warnings.push(MoveWarning::CodeFenceSuspected {
                        path: affected_path,
                        count,
                    });
                }
            }
        }
    }

    // 3. Outgoing relative-path link: the source document contains a Markdown
    //    link with a relative path that will break after moving directories.
    if let Some(src_doc) = index.documents.iter().find(|d| d.path == *src_rel) {
        let src_parent = src_rel.parent();
        let dst_parent = dst_rel.parent();
        if src_parent != dst_parent {
            for link in &src_doc.links {
                if link.raw.starts_with('[')
                    && (link.raw.contains("](../") || link.raw.contains("](./"))
                {
                    warnings.push(MoveWarning::OutgoingRelativePathLink {
                        source: src_rel.clone(),
                        raw: link.raw.clone(),
                    });
                }
            }
        }
    }

    warnings
}

#[allow(dead_code)] // called only by collect_warnings; both deleted in Task 20
fn count_stem_in_code_fences(body: &str, stem: &str) -> usize {
    let mut in_fence = false;
    let mut count = 0;
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence && line.contains(stem) {
            count += 1;
        }
    }
    count
}

/// Build a `MoveReport` from a `RepairPlan` produced by `preflight_and_plan`.
///
/// `applied` should be `false` when rendering the preview (before mutation) and
/// `true` after `apply_repair_plan` has completed.
pub fn build_report(plan: &RepairPlan, applied: bool, warnings: Vec<MoveWarning>) -> MoveReport {
    use std::collections::BTreeMap;

    let move_op = plan
        .changes
        .iter()
        .find(|c| c.operation == "move_document")
        .expect("plan must contain exactly one move_document op");

    // Aggregate affected links from the move op's link_risk (NOT from
    // separate rewrite_link ops — the move-cascade pattern uses link_risk).
    let mut counts: BTreeMap<Utf8PathBuf, usize> = BTreeMap::new();
    let mut total = 0;
    if let Some(risk) = &move_op.link_risk {
        for affected in risk
            .stem_links
            .iter()
            .chain(risk.path_qualified_wikilinks.iter())
            .chain(risk.markdown_links.iter())
        {
            *counts.entry(affected.source_path.clone()).or_insert(0) += 1;
            total += 1;
        }
    }

    let files = counts
        .into_iter()
        .map(|(path, count)| LinkFile { path, count })
        .collect();

    MoveReport {
        schema_version: 1,
        operation: "move".into(),
        source: move_op.path.clone(),
        destination: move_op
            .destination
            .clone()
            .expect("move_document.destination must be set"),
        link_rewrites: LinkSummary { total, files },
        applied,
        warnings,
    }
}

/// Resolve a source specifier to a vault-relative path.
///
/// Accepts an exact vault-relative path or a bare stem. Returns
/// `MovePreflightError::SourceMissing` or `MovePreflightError::SourceAmbiguous`
/// on failure.
fn resolve_src(index: &GraphIndex, src: &str) -> Result<Utf8PathBuf, MovePreflightError> {
    // Exact path match takes priority.
    if let Some(doc) = index.documents.iter().find(|d| d.path == src) {
        return Ok(doc.path.clone());
    }

    // Stem match (case-insensitive).
    let candidates: Vec<Utf8PathBuf> = index
        .documents
        .iter()
        .filter(|d| d.stem.eq_ignore_ascii_case(src))
        .map(|d| d.path.clone())
        .collect();

    match candidates.as_slice() {
        [single] => Ok(single.clone()),
        [] => Err(MovePreflightError::SourceMissing(src.to_string())),
        _ => Err(MovePreflightError::SourceAmbiguous {
            stem: src.to_string(),
            candidates,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal on-disk vault with two documents and return the temp dir,
    /// vault root, and GraphIndex. Layout:
    ///   a.md  — contains `[[b]]` backlink to b.md
    ///   b.md  — no outgoing links
    fn fixture_vault() -> (tempfile::TempDir, Utf8PathBuf, GraphIndex) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-move-preflight-")
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

        let index = crate::graph::build_index(&root).unwrap();
        (tmp, root, index)
    }

    #[test]
    fn move_preflight_source_missing_errors() {
        let (_tmp, root, index) = fixture_vault();
        let err = preflight_and_plan(PreflightConfig {
            src: "nope.md",
            dst: "renamed.md",
            force: false,
            no_link_rewrite: false,
            vault_root: &root,
            index: &index,
        })
        .unwrap_err();
        assert!(
            matches!(err, MovePreflightError::SourceMissing(_)),
            "expected SourceMissing, got: {err}"
        );
    }

    #[test]
    fn move_preflight_destination_exists_without_force() {
        let (_tmp, root, index) = fixture_vault();
        let err = preflight_and_plan(PreflightConfig {
            src: "a.md",
            dst: "b.md",
            force: false,
            no_link_rewrite: false,
            vault_root: &root,
            index: &index,
        })
        .unwrap_err();
        assert!(
            matches!(err, MovePreflightError::DestinationExists(_)),
            "expected DestinationExists, got: {err}"
        );
    }

    #[test]
    fn move_preflight_same_path_errors() {
        let (_tmp, root, index) = fixture_vault();
        let err = preflight_and_plan(PreflightConfig {
            src: "a.md",
            dst: "a.md",
            force: true, // force can't override same-path
            no_link_rewrite: false,
            vault_root: &root,
            index: &index,
        })
        .unwrap_err();
        assert!(
            matches!(err, MovePreflightError::SamePath(_)),
            "expected SamePath, got: {err}"
        );
    }

    #[test]
    fn move_preflight_destination_parent_missing() {
        let (_tmp, root, index) = fixture_vault();
        let err = preflight_and_plan(PreflightConfig {
            src: "a.md",
            dst: "notes/b.md", // notes/ doesn't exist
            force: false,
            no_link_rewrite: false,
            vault_root: &root,
            index: &index,
        })
        .unwrap_err();
        assert!(
            matches!(err, MovePreflightError::DestinationParentMissing(_)),
            "expected DestinationParentMissing, got: {err}"
        );
    }

    #[test]
    fn move_plan_synthesizes_one_move_with_link_risk() {
        let (_tmp, root, index) = fixture_vault();
        // Move b.md → renamed.md; a.md has [[b]] so link_risk should be Some
        // and carry at least one affected link record.
        let plan = preflight_and_plan(PreflightConfig {
            src: "b.md",
            dst: "renamed.md",
            force: false,
            no_link_rewrite: false,
            vault_root: &root,
            index: &index,
        })
        .unwrap();

        // Exactly one PlannedChange — the move_document op.
        assert_eq!(plan.changes.len(), 1, "expected exactly one PlannedChange");
        assert_eq!(plan.changes[0].operation, "move_document");
        assert_eq!(plan.changes[0].path, Utf8PathBuf::from("b.md"));
        assert_eq!(
            plan.changes[0].destination,
            Some(Utf8PathBuf::from("renamed.md"))
        );

        // link_risk is populated and carries at least one affected link.
        let risk = plan.changes[0]
            .link_risk
            .as_ref()
            .expect("link_risk must be Some when --no-link-rewrite is false");
        let total_affected =
            risk.stem_links.len() + risk.path_qualified_wikilinks.len() + risk.markdown_links.len();
        assert!(
            total_affected >= 1,
            "expected at least one affected link in link_risk (a.md has [[b]])"
        );

        assert_eq!(plan.summary.planned_changes, 1);
    }

    // ---------------------------------------------------------------------------
    // MoveReport / renderer tests
    // ---------------------------------------------------------------------------

    #[test]
    fn move_report_json_shape_matches_spec() {
        let report = MoveReport {
            schema_version: 1,
            operation: "move".into(),
            source: "foo.md".into(),
            destination: "notes/bar.md".into(),
            link_rewrites: LinkSummary {
                total: 3,
                files: vec![
                    LinkFile {
                        path: "a.md".into(),
                        count: 2,
                    },
                    LinkFile {
                        path: "b.md".into(),
                        count: 1,
                    },
                ],
            },
            applied: false,
            warnings: Vec::new(),
        };
        let json = serde_json::to_string_pretty(&report).unwrap();
        assert!(json.contains(r#""operation": "move""#));
        assert!(json.contains(r#""source": "foo.md""#));
        assert!(json.contains(r#""destination": "notes/bar.md""#));
        assert!(json.contains(r#""total": 3"#));
        assert!(json.contains(r#""applied": false"#));
    }

    #[test]
    fn move_report_records_shape_renders_summary() {
        let report = MoveReport {
            schema_version: 1,
            operation: "move".into(),
            source: "foo.md".into(),
            destination: "notes/bar.md".into(),
            link_rewrites: LinkSummary {
                total: 3,
                files: vec![
                    LinkFile {
                        path: "a.md".into(),
                        count: 2,
                    },
                    LinkFile {
                        path: "b.md".into(),
                        count: 1,
                    },
                ],
            },
            applied: false,
            warnings: Vec::new(),
        };
        let mut buf = Vec::new();
        render_records(&mut buf, &report).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("norn move foo.md → notes/bar.md"));
        assert!(out.contains("3 backlinks to rewrite across 2 files"));
    }

    #[test]
    fn move_report_records_applied_uses_past_tense() {
        let report = MoveReport {
            schema_version: 1,
            operation: "move".into(),
            source: "foo.md".into(),
            destination: "notes/bar.md".into(),
            link_rewrites: LinkSummary {
                total: 0,
                files: Vec::new(),
            },
            applied: true,
            warnings: Vec::new(),
        };
        let mut buf = Vec::new();
        render_records(&mut buf, &report).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("✓ moved foo.md → notes/bar.md"));
    }

    #[test]
    fn build_report_counts_affected_links_per_source() {
        let (_tmp, root, index) = fixture_vault();
        let plan = preflight_and_plan(PreflightConfig {
            src: "b.md",
            dst: "renamed.md",
            force: false,
            no_link_rewrite: false,
            vault_root: &root,
            index: &index,
        })
        .unwrap();
        let report = build_report(&plan, false, Vec::new());
        assert_eq!(report.operation, "move");
        assert_eq!(report.source.as_str(), "b.md");
        assert_eq!(report.destination.as_str(), "renamed.md");
        assert!(report.link_rewrites.total >= 1);
        assert!(!report.applied);
    }

    #[test]
    fn build_report_no_link_rewrite_zero_total() {
        let (_tmp, root, index) = fixture_vault();
        let plan = preflight_and_plan(PreflightConfig {
            src: "b.md",
            dst: "renamed.md",
            force: false,
            no_link_rewrite: true,
            vault_root: &root,
            index: &index,
        })
        .unwrap();
        let report = build_report(&plan, false, Vec::new());
        assert_eq!(report.link_rewrites.total, 0);
    }

    #[test]
    fn move_plan_no_link_rewrite_omits_link_risk() {
        let (_tmp, root, index) = fixture_vault();
        let plan = preflight_and_plan(PreflightConfig {
            src: "b.md",
            dst: "renamed.md",
            force: false,
            no_link_rewrite: true,
            vault_root: &root,
            index: &index,
        })
        .unwrap();

        // Exactly one PlannedChange — the move_document op.
        assert_eq!(plan.changes.len(), 1, "expected exactly one PlannedChange");
        assert_eq!(plan.changes[0].operation, "move_document");

        // link_risk must be None when --no-link-rewrite is set.
        assert!(
            plan.changes[0].link_risk.is_none(),
            "expected link_risk to be None when --no-link-rewrite is true"
        );

        assert_eq!(plan.summary.planned_changes, 1);
    }

    // ---------------------------------------------------------------------------
    // collect_warnings tests
    // ---------------------------------------------------------------------------

    #[test]
    fn collect_warnings_stem_collision_when_destination_stem_exists_elsewhere() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-move-warn-stem-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path())
            .unwrap()
            .to_path_buf();
        std::fs::create_dir_all(tmp.path().join(".norn")).unwrap();
        std::fs::write(tmp.path().join(".norn/config.yaml"), "validate: {}\n").unwrap();
        std::fs::create_dir(root.join("subdir")).unwrap();
        std::fs::write(root.join("source.md"), "---\ntype: note\n---\n# S\n").unwrap();
        std::fs::write(root.join("renamed.md"), "---\ntype: note\n---\n# R1\n").unwrap();
        let index = crate::graph::build_index(&root).unwrap();

        let plan = preflight_and_plan(PreflightConfig {
            src: "source.md",
            dst: "subdir/renamed.md",
            force: false,
            no_link_rewrite: true,
            vault_root: &root,
            index: &index,
        })
        .expect("preflight should pass");

        let warnings = collect_warnings(&plan, &index, &root);
        assert!(
            warnings
                .iter()
                .any(|w| matches!(w, MoveWarning::StemCollision { stem, .. } if stem == "renamed")),
            "stem collision warning expected: {warnings:?}"
        );
    }

    #[test]
    fn collect_warnings_empty_when_unique_stem_same_dir() {
        let (_tmp, root, index) = fixture_vault();
        let plan = preflight_and_plan(PreflightConfig {
            src: "b.md",
            dst: "unique-name.md",
            force: false,
            no_link_rewrite: true,
            vault_root: &root,
            index: &index,
        })
        .unwrap();
        let warnings = collect_warnings(&plan, &index, &root);
        assert!(warnings.is_empty(), "no warnings expected: {warnings:?}");
    }
}
