//! Expand a high-level `move_folder` op into N `move_document` ApplyOp
//! ops — one per `.md` file under `src`, preserving subdirectory structure
//! under `dst`. `parents` propagates so intermediate dst subdirs are created
//! at apply time. Each expanded op gets `link_risk` populated via classify.

use crate::domain::GraphIndex;
use crate::standards::apply::ApplyError;
use crate::standards::{classify_link_risk, ApplyOp};
use anyhow::Result;
use std::collections::BTreeSet;

pub(crate) struct MoveFolderOp {
    pub src: String,
    pub dst: String,
    pub parents: bool,
    pub force: bool,
    pub no_link_rewrite: bool,
}

/// Walk the vault index for `.md` files whose path starts with `op.src/`,
/// produce one `move_document` ApplyOp per file (preserving relative
/// subdirectory structure under `op.dst`), and populate `link_risk` for each.
pub(crate) fn expand_move_folder(op: &MoveFolderOp, index: &GraphIndex) -> Result<Vec<ApplyOp>> {
    // Normalise the src prefix to include a trailing slash so we don't
    // accidentally match "src_dir2" when op.src is "src_dir".
    let src_prefix = if op.src.ends_with('/') {
        op.src.clone()
    } else {
        format!("{}/", op.src)
    };

    // Reject a destination that lands inside the source's own subtree — a move of
    // a directory into itself (`move a a/z`). Such an expansion emits self-nesting
    // ops (`a/x.md → a/z/x.md`, `a/z/y.md → a/z/z/y.md`, …) that a confirmed apply
    // either garbles into a recursively nested tree or partially fails on a
    // collision with a not-yet-vacated source — and the dry-run forecast, whose
    // occupant is itself a move source, cannot foresee either. Refuse up front at
    // expansion (index-only, no filesystem), so the verdict is identical on the
    // dry-run forecast and the confirmed apply. `--force` does not bypass: moving a
    // directory into itself is logically impossible, not an overwrite decision.
    let dst_prefix = if op.dst.ends_with('/') {
        op.dst.clone()
    } else {
        format!("{}/", op.dst)
    };
    if dst_prefix != src_prefix && dst_prefix.starts_with(&src_prefix) {
        return Err(ApplyError::MoveDestinationInsideSource {
            src: op.src.clone().into(),
            destination: op.dst.clone().into(),
        }
        .into());
    }

    // The set of vault-relative source paths this folder move vacates. A
    // destination that lands on one of these is not a collision — the occupant is
    // itself moving out of the way — so it is excluded from the collision check.
    let sources: BTreeSet<&str> = index
        .documents
        .iter()
        .map(|doc| doc.path.as_str())
        .filter(|rel| rel.starts_with(&src_prefix))
        .collect();

    // Every indexed document path, hashed once, so the per-document
    // destination-collision check below is an O(1) membership test rather than an
    // O(n) scan inside the O(n) expansion loop (O(n²) total on a large folder).
    let known_paths: std::collections::HashSet<&str> = index
        .documents
        .iter()
        .map(|doc| doc.path.as_str())
        .collect();

    let mut changes = Vec::new();

    for doc in &index.documents {
        let rel = doc.path.as_str();

        // Filter to docs under op.src (path starts with src_prefix).
        if !rel.starts_with(&src_prefix) {
            continue;
        }

        // Strip the src prefix to get the path relative to src_dir.
        let suffix = &rel[src_prefix.len()..];

        // Compute the destination path: dst_dir/<suffix>
        let new_rel_str = if op.dst.ends_with('/') {
            format!("{}{}", op.dst, suffix)
        } else {
            format!("{}/{}", op.dst, suffix)
        };

        let old_rel = doc.path.clone();
        let new_rel: camino::Utf8PathBuf = new_rel_str.into();

        // Destination-collision forecast (NRN-161): a destination already occupied
        // by a KNOWN vault document (one not itself being vacated by this move) is
        // a collision the index can see, so refuse it here — at expansion, which
        // runs on the dry-run forecast too — rather than only when `apply_move`
        // hits the live file. Consulting the index (never the filesystem) keeps
        // this a forecastable refusal; a destination occupied by something the
        // index does not track (a non-vault file, an ignored path) is knowledge
        // only the live filesystem carries and stays an apply-time refusal
        // (`apply_move`). `--force` overwrites, so it is not a collision.
        if !op.force
            && new_rel != old_rel
            && !sources.contains(new_rel.as_str())
            && known_paths.contains(new_rel.as_str())
        {
            return Err(ApplyError::MoveDestinationExists {
                destination: new_rel,
            }
            .into());
        }

        // `no_link_rewrite` suppresses the backlink cascade for every expanded
        // op, matching the single-document move dispatch (`intent::expand`).
        let link_risk = if op.no_link_rewrite {
            None
        } else {
            Some(classify_link_risk(
                &old_rel,
                &new_rel,
                &index.documents,
                &index.files,
            ))
        };

        let change = ApplyOp {
            change_id: format!("move-{}", old_rel),
            path: old_rel,
            document_hash: doc.hash.clone(),
            finding_code: None,
            finding_rule: None,
            repair_rule: None,
            operation: "move_document".into(),
            field: None,
            expected_old_value: None,
            new_value: None,
            destination: Some(new_rel),
            link_risk,
            warnings: Vec::new(),
            force: op.force,
            parents: op.parents,
        };

        changes.push(change);
    }

    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn synth_vault() -> TempDir {
        let tmp = tempfile::Builder::new()
            .prefix("planner-move-folder-")
            .tempdir()
            .unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src_dir/sub")).unwrap();
        std::fs::write(
            root.join("src_dir/a.md"),
            "---\ntype: note\n---\n# A\n[[b]]\n",
        )
        .unwrap();
        std::fs::write(root.join("src_dir/sub/b.md"), "---\ntype: note\n---\n# B\n").unwrap();
        std::fs::write(root.join("c.md"), "---\ntype: note\n---\n# C\n[[a]]\n").unwrap();
        tmp
    }

    #[test]
    fn expand_move_folder_produces_one_op_per_md_file_under_src() {
        let tmp = synth_vault();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let index = crate::graph::build_index(root).unwrap();

        let op = MoveFolderOp {
            src: "src_dir".into(),
            dst: "dst_dir".into(),
            parents: true,
            force: false,
            no_link_rewrite: false,
        };
        let expanded = expand_move_folder(&op, &index).unwrap();
        assert_eq!(
            expanded.len(),
            2,
            "expected 2 move_document ops from 2 .md files in src_dir"
        );

        for change in &expanded {
            assert_eq!(change.operation, "move_document");
            assert!(
                change
                    .destination
                    .as_ref()
                    .unwrap()
                    .as_str()
                    .starts_with("dst_dir/"),
                "destination should be under dst_dir, got {:?}",
                change.destination
            );
            assert!(
                change.link_risk.is_some(),
                "link_risk must be populated by planner"
            );
            assert!(change.parents, "parents flag must propagate");
            assert!(!change.force, "force defaults off when the flag is unset");
        }

        // Verify structure-preserving move: src_dir/sub/b.md → dst_dir/sub/b.md
        let b_op = expanded
            .iter()
            .find(|c| c.path == "src_dir/sub/b.md")
            .expect("should have a move op for src_dir/sub/b.md");
        assert_eq!(
            b_op.destination.as_deref().map(|p| p.as_str()),
            Some("dst_dir/sub/b.md")
        );
    }

    #[test]
    fn expand_move_folder_threads_force_and_no_link_rewrite_to_every_op() {
        let tmp = synth_vault();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let index = crate::graph::build_index(root).unwrap();

        let op = MoveFolderOp {
            src: "src_dir".into(),
            dst: "dst_dir".into(),
            parents: true,
            force: true,
            no_link_rewrite: true,
        };
        let expanded = expand_move_folder(&op, &index).unwrap();
        assert_eq!(expanded.len(), 2);
        for change in &expanded {
            assert!(change.force, "force must propagate to every expanded op");
            assert!(
                change.link_risk.is_none(),
                "no_link_rewrite must suppress the backlink cascade on every op"
            );
        }
    }

    #[test]
    fn expand_move_folder_refuses_destination_inside_source_subtree() {
        // `move a a/z`: the destination is inside the source's own subtree, a
        // move-into-self. Expansion must refuse up front rather than emit
        // self-nesting ops. `--force` does not bypass.
        let tmp = tempfile::Builder::new()
            .prefix("planner-move-into-self-")
            .tempdir()
            .unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("a/z")).unwrap();
        std::fs::write(root.join("a/x.md"), "---\ntype: note\n---\n# X\n").unwrap();
        std::fs::write(root.join("a/z/y.md"), "---\ntype: note\n---\n# Y\n").unwrap();
        let uroot = camino::Utf8Path::from_path(root).unwrap();
        let index = crate::graph::build_index(uroot).unwrap();

        for force in [false, true] {
            let op = MoveFolderOp {
                src: "a".into(),
                dst: "a/z".into(),
                parents: true,
                force,
                no_link_rewrite: false,
            };
            let err = expand_move_folder(&op, &index)
                .expect_err("move-into-self must refuse at expansion");
            let apply_err = err
                .downcast_ref::<ApplyError>()
                .expect("a typed ApplyError refusal");
            assert_eq!(apply_err.code(), "move-destination-inside-source");
        }

        // Moving a subtree OUT is unaffected.
        let out = MoveFolderOp {
            src: "a/z".into(),
            dst: "a".into(),
            parents: true,
            force: false,
            no_link_rewrite: false,
        };
        assert!(
            expand_move_folder(&out, &index).is_ok(),
            "moving a subtree out of its parent is a valid move"
        );
    }
}
