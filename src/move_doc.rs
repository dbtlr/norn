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

impl MovePreflightError {
    /// The stable, machine-branchable kebab code for this preflight refusal
    /// (NRN-229), so an MCP `vault.move` consumer / a `--format json` CLI caller
    /// branches on the code rather than string-matching the prose. `Display` is
    /// unchanged (byte-identical CLI/stderr output); the code rides alongside.
    ///
    /// Reuses existing codes where the semantic is genuinely identical (NRN-235,
    /// one-semantic-one-code): `target-not-found` / `target-ambiguous` are `set`'s
    /// primary-target resolution codes (the move SOURCE not resolving to an
    /// indexed document is the same condition), and `destination-exists` /
    /// `parent-missing` are `new`'s destination codes (a move destination that
    /// already exists, or whose parent dir is absent, is the same condition
    /// `new` refuses on). Only the no-op same-path case is move-specific.
    ///
    /// NOTE: distinct from the applier's apply-time `move-source-missing` /
    /// `move-destination-exists` lifecycle codes — those fire when the file is
    /// missing/present on the FILESYSTEM at apply time; these fire earlier, when
    /// the argument does not resolve against the graph index.
    pub fn code(&self) -> &'static str {
        match self {
            MovePreflightError::SourceMissing(_) => "target-not-found",
            MovePreflightError::SourceAmbiguous { .. } => "target-ambiguous",
            MovePreflightError::DestinationExists(_) => "destination-exists",
            MovePreflightError::DestinationParentMissing(_) => "parent-missing",
            MovePreflightError::SamePath(_) => "source-destination-same",
        }
    }
}

pub(crate) struct PreflightConfig<'a> {
    pub src: &'a str,
    pub dst: &'a str,
    pub force: bool,
    pub no_link_rewrite: bool,
    /// Mirrors `-p` / `--parents`: when true, a missing destination parent
    /// directory is not refused here — it is created inside the applier as
    /// part of the audited apply step (NRN-234). Threaded onto the resulting
    /// `move_document` PlannedChange's `parents` field so the applier knows
    /// to create it.
    pub parents: bool,
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

    // Destination parent must exist — unless `--parents` was passed, in which
    // case the applier creates it as part of the audited apply step (NRN-234).
    if !cfg.parents {
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
        parents: cfg.parents,
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
            parents: false,
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
            parents: false,
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
            parents: false,
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
            parents: false,
            vault_root: &root,
            index: &index,
        })
        .unwrap_err();
        assert!(
            matches!(err, MovePreflightError::DestinationParentMissing(_)),
            "expected DestinationParentMissing, got: {err}"
        );
    }

    /// NRN-234: with `parents: true`, preflight must NOT refuse a missing
    /// destination parent — that directory is created inside the applier as
    /// part of the audited apply step, not pre-created (or refused) here. The
    /// resulting `move_document` change carries `parents: true` so the applier
    /// knows to create it.
    #[test]
    fn move_preflight_parents_true_skips_destination_parent_check() {
        let (_tmp, root, index) = fixture_vault();
        let plan = preflight_and_plan(PreflightConfig {
            src: "a.md",
            dst: "notes/b.md", // notes/ doesn't exist
            force: false,
            no_link_rewrite: false,
            parents: true,
            vault_root: &root,
            index: &index,
        })
        .expect("parents: true must not refuse a missing destination parent");

        assert!(
            !root.join("notes").as_std_path().exists(),
            "preflight must not create the destination parent itself"
        );
        assert!(
            plan.changes[0].parents,
            "the move_document change must carry parents: true for the applier"
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
            parents: false,
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

    #[test]
    fn move_plan_no_link_rewrite_omits_link_risk() {
        let (_tmp, root, index) = fixture_vault();
        let plan = preflight_and_plan(PreflightConfig {
            src: "b.md",
            dst: "renamed.md",
            force: false,
            no_link_rewrite: true,
            parents: false,
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

    /// NRN-229: every `MovePreflightError` variant carries a stable kebab
    /// `.code()`. Source-resolution failures REUSE `set`'s target codes and
    /// destination collisions REUSE `new`'s destination codes
    /// (one-semantic-one-code); only the no-op same-path case is move-specific.
    #[test]
    fn move_preflight_error_codes_are_stable_and_reuse_shared_semantics() {
        use camino::Utf8PathBuf;
        let cases: Vec<(MovePreflightError, &str)> = vec![
            (
                MovePreflightError::SourceMissing("x".into()),
                "target-not-found",
            ),
            (
                MovePreflightError::SourceAmbiguous {
                    stem: "x".into(),
                    candidates: vec![],
                },
                "target-ambiguous",
            ),
            (
                MovePreflightError::DestinationExists(Utf8PathBuf::from("x.md")),
                "destination-exists",
            ),
            (
                MovePreflightError::DestinationParentMissing(Utf8PathBuf::from("d")),
                "parent-missing",
            ),
            (
                MovePreflightError::SamePath(Utf8PathBuf::from("x.md")),
                "source-destination-same",
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
