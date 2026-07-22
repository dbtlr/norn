//! Expand a high-level `move_folder` op into N `move_document` ApplyOp
//! ops — one per `.md` file under `src`, preserving subdirectory structure
//! under `dst`. `parents` propagates so intermediate dst subdirs are created
//! at apply time. Each expanded op gets `link_risk` populated via classify.

use crate::domain::GraphIndex;
use crate::standards::{classify_link_risk, ApplyOp};
use anyhow::Result;

pub(crate) struct MoveFolderOp {
    pub src: String,
    pub dst: String,
    pub parents: bool,
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

        let link_risk = classify_link_risk(&old_rel, &new_rel, &index.documents, &index.files);

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
            link_risk: Some(link_risk),
            warnings: Vec::new(),
            force: false,
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
}
