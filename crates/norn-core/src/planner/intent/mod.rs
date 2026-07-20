//! Intent op vocabulary + dispatch to per-kind expanders.
//!
//! Per-kind expanders land in submodules (Plan Tasks 4, 5). The dispatcher
//! in this file (Plan Task 6) routes high-level ops to expanders and
//! passes low-level ops through with planner-filled link_risk.

use crate::domain::GraphIndex;
use crate::plan::MigrationOp;
use crate::standards::{classify_link_risk, PlannedChange};
use anyhow::{anyhow, Result};
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

pub mod move_folder;
pub mod rewrite_wikilink;

/// The set of op kinds the planner expands (vs. passes through to the applier).
pub const HIGH_LEVEL_KINDS: &[&str] = &["move_folder", "rewrite_wikilink"];

/// Typed view of intent fields for high-level op kinds. Used internally by
/// expanders; the on-disk schema uses MigrationOp with `fields: serde_json::Value`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentOp {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub src: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub dst: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub old: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub new: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub parents: Option<bool>,
}

impl IntentOp {
    pub fn is_high_level(&self) -> bool {
        HIGH_LEVEL_KINDS.contains(&self.kind.as_str())
    }
}

/// Single dispatch entry point: converts any `MigrationOp` into a
/// `Vec<PlannedChange>` ready for the applier.
///
/// - **High-level kinds** (`move_folder`, `rewrite_wikilink`): dispatch to
///   the corresponding expander.
/// - **Low-level move/delete** (`move_document`, `delete_document`): pass
///   through with `link_risk` populated by `classify_link_risk`.
/// - **Other low-level kinds** (`set_frontmatter`, `add_frontmatter`,
///   `remove_frontmatter`, `rewrite_link`, `replace_body`, `create_document`):
///   pass through by deserializing `op.fields` into a `PlannedChange`.
/// - **Unknown kind**: returns `Err`.
pub(crate) fn expand(op: &MigrationOp, index: &GraphIndex) -> Result<Vec<PlannedChange>> {
    match op.kind.as_str() {
        "move_folder" => {
            let src = op.fields["src"]
                .as_str()
                .ok_or_else(|| anyhow!("move_folder missing src"))?;
            let dst = op.fields["dst"]
                .as_str()
                .ok_or_else(|| anyhow!("move_folder missing dst"))?;
            let parents = op.fields["parents"].as_bool().unwrap_or(false);
            move_folder::expand_move_folder(
                &move_folder::MoveFolderOp {
                    src: src.into(),
                    dst: dst.into(),
                    parents,
                },
                index,
            )
        }

        "rewrite_wikilink" => {
            let old = op.fields["old"]
                .as_str()
                .ok_or_else(|| anyhow!("rewrite_wikilink missing old"))?;
            let new = op.fields["new"]
                .as_str()
                .ok_or_else(|| anyhow!("rewrite_wikilink missing new"))?;
            rewrite_wikilink::expand_rewrite_wikilink(
                &rewrite_wikilink::RewriteWikilinkOp {
                    old: old.into(),
                    new: new.into(),
                },
                index,
            )
        }

        "move_document" => {
            let src = op.fields["src"]
                .as_str()
                .ok_or_else(|| anyhow!("move_document missing src"))?;
            let dst = op.fields["dst"]
                .as_str()
                .ok_or_else(|| anyhow!("move_document missing dst"))?;
            let parents = op.fields["parents"].as_bool().unwrap_or(false);
            let force = op.fields["force"].as_bool().unwrap_or(false);
            let no_link_rewrite = op.fields["no_link_rewrite"].as_bool().unwrap_or(false);

            let old_path: Utf8PathBuf = src.into();
            let new_path: Utf8PathBuf = dst.into();
            let link_risk = if no_link_rewrite {
                None
            } else {
                Some(classify_link_risk(
                    &old_path,
                    &new_path,
                    &index.documents,
                    &index.files,
                ))
            };

            let change = PlannedChange {
                change_id: format!("move-{}", src),
                path: old_path,
                document_hash: String::new(),
                finding_code: "operator-request".into(),
                finding_rule: None,
                repair_rule: "operator-request".into(),
                operation: "move_document".into(),
                field: None,
                expected_old_value: None,
                new_value: None,
                destination: Some(new_path),
                link_risk,
                warnings: Vec::new(),
                force,
                parents,
            };
            Ok(vec![change])
        }

        "delete_document" => {
            let path = op.fields["path"]
                .as_str()
                .ok_or_else(|| anyhow!("delete_document missing path"))?;

            let doc_path: Utf8PathBuf = path.into();

            // Only populate link_risk when rewrite_to is present.
            let link_risk = op.fields["rewrite_to"].as_str().map(|rewrite_to| {
                let rewrite_path: Utf8PathBuf = rewrite_to.into();
                classify_link_risk(&doc_path, &rewrite_path, &index.documents, &index.files)
            });

            let change = PlannedChange {
                change_id: format!("delete-{}", path),
                path: doc_path,
                document_hash: String::new(),
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
            Ok(vec![change])
        }

        "set_frontmatter" | "add_frontmatter" | "remove_frontmatter" | "rewrite_link"
        | "replace_body" | "create_document" => {
            // Pass through: deserialize op.fields into PlannedChange.
            // Insert required fields that have no #[serde(default)] if absent.
            let mut map = op
                .fields
                .as_object()
                .cloned()
                .ok_or_else(|| anyhow!("op.fields for {} must be an object", op.kind))?;
            // Insert the operation kind so PlannedChange.operation is populated.
            map.entry("operation")
                .or_insert_with(|| serde_json::Value::String(op.kind.clone()));
            // Insert defaults for required non-defaulted fields when missing.
            // Extract path first (outside the closures) to avoid borrow conflicts.
            let path_for_id = map
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_owned();
            let default_change_id = format!("{}-{}", op.kind, path_for_id);
            map.entry("change_id")
                .or_insert_with(|| serde_json::Value::String(default_change_id));
            map.entry("document_hash")
                .or_insert_with(|| serde_json::Value::String(String::new()));
            map.entry("finding_code")
                .or_insert_with(|| serde_json::Value::String("operator-request".into()));
            map.entry("repair_rule")
                .or_insert_with(|| serde_json::Value::String("operator-request".into()));

            let change: PlannedChange = serde_json::from_value(serde_json::Value::Object(map))?;
            Ok(vec![change])
        }

        "str_replace"
        | "replace_section"
        | "append_to_section"
        | "delete_section"
        | "insert_before_heading"
        | "insert_after_heading" => {
            // Section/body edit ops (NRN-98 / H1). The op fields ARE the
            // `EditOp` (internally tagged on `op`); carry that payload in
            // `new_value` so the applier can deserialize and apply it against
            // the current body at apply time, under whole-doc CAS. Extra keys
            // (`path`/`document_hash`) are ignored by `EditOp`'s deserializer.
            let path = op.fields["path"]
                .as_str()
                .ok_or_else(|| anyhow!("{} missing path", op.kind))?;
            let document_hash = op.fields["document_hash"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let mut edit_json = op
                .fields
                .as_object()
                .cloned()
                .ok_or_else(|| anyhow!("op.fields for {} must be an object", op.kind))?;
            edit_json.insert("op".into(), serde_json::Value::String(op.kind.clone()));

            // change_id must be unique per op: two edits of the same kind on the
            // same document would otherwise collide and clobber each other's
            // telemetry span. Prefer the plan-supplied `op.id`; else discriminate
            // by a hash of the edit payload.
            let change_id = op.id.clone().unwrap_or_else(|| {
                let payload = serde_json::Value::Object(edit_json.clone()).to_string();
                let digest = blake3::hash(payload.as_bytes()).to_hex();
                format!("{}-{}-{}", op.kind, path, &digest[..8])
            });

            let change = PlannedChange {
                change_id,
                path: path.into(),
                document_hash,
                finding_code: "operator-request".into(),
                finding_rule: None,
                repair_rule: "operator-request".into(),
                operation: op.kind.clone(),
                field: None,
                expected_old_value: None,
                new_value: Some(serde_json::Value::Object(edit_json)),
                destination: None,
                link_risk: None,
                warnings: Vec::new(),
                force: false,
                parents: false,
            };
            Ok(vec![change])
        }

        other => Err(anyhow!("unknown operation kind: {}", other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intent_op_high_level_kinds() {
        let op = IntentOp {
            kind: "move_folder".into(),
            src: Some("a/".into()),
            dst: Some("b/".into()),
            old: None,
            new: None,
            parents: Some(true),
        };
        assert!(op.is_high_level());

        let op2 = IntentOp {
            kind: "rewrite_wikilink".into(),
            src: None,
            dst: None,
            old: Some("foo".into()),
            new: Some("bar".into()),
            parents: None,
        };
        assert!(op2.is_high_level());
    }

    #[test]
    fn intent_op_low_level_kinds_recognized() {
        let op = IntentOp {
            kind: "move_document".into(),
            src: Some("a.md".into()),
            dst: Some("b.md".into()),
            old: None,
            new: None,
            parents: None,
        };
        assert!(!op.is_high_level());

        for low_kind in &[
            "set_frontmatter",
            "delete_document",
            "rewrite_link",
            "new_document",
            "replace_body",
        ] {
            let op = IntentOp {
                kind: (*low_kind).into(),
                src: None,
                dst: None,
                old: None,
                new: None,
                parents: None,
            };
            assert!(!op.is_high_level(), "{} should be low-level", low_kind);
        }
    }
}

#[cfg(test)]
mod expansion_tests {
    use super::*;
    use crate::plan::MigrationOp;
    use tempfile::TempDir;

    fn synth_vault() -> TempDir {
        let tmp = tempfile::Builder::new()
            .prefix("planner-dispatch-")
            .tempdir()
            .unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.md"), "---\ntype: note\n---\n# A\n[[b]]\n").unwrap();
        std::fs::write(root.join("b.md"), "---\ntype: note\n---\n# B\n").unwrap();
        std::fs::create_dir_all(root.join("src_dir")).unwrap();
        std::fs::write(root.join("src_dir/c.md"), "---\ntype: note\n---\n# C\n").unwrap();
        tmp
    }

    #[test]
    fn dispatch_high_level_move_folder() {
        let tmp = synth_vault();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let index = crate::graph::build_index(root).unwrap();

        let op = MigrationOp {
            kind: "move_folder".into(),
            id: None,
            requires: vec![],
            fields: serde_json::json!({"src": "src_dir", "dst": "dst_dir", "parents": true}),
            footnote: None,
        };
        let expanded = expand(&op, &index).unwrap();
        assert!(!expanded.is_empty());
        assert!(expanded.iter().all(|c| c.operation == "move_document"));
    }

    #[test]
    fn dispatch_low_level_move_document_fills_link_risk() {
        let tmp = synth_vault();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let index = crate::graph::build_index(root).unwrap();

        let op = MigrationOp {
            kind: "move_document".into(),
            id: None,
            requires: vec![],
            fields: serde_json::json!({"src": "b.md", "dst": "renamed.md"}),
            footnote: None,
        };
        let expanded = expand(&op, &index).unwrap();
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0].operation, "move_document");
        assert!(
            expanded[0].link_risk.is_some(),
            "low-level move_document gets link_risk filled by planner"
        );
    }

    #[test]
    fn dispatch_low_level_delete_document_without_rewrite_to_has_no_link_risk() {
        let tmp = synth_vault();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let index = crate::graph::build_index(root).unwrap();

        let op = MigrationOp {
            kind: "delete_document".into(),
            id: None,
            requires: vec![],
            fields: serde_json::json!({"path": "a.md"}),
            footnote: None,
        };
        let expanded = expand(&op, &index).unwrap();
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0].operation, "delete_document");
        assert!(
            expanded[0].link_risk.is_none(),
            "delete_document without rewrite_to should have no link_risk"
        );
    }

    #[test]
    fn dispatch_low_level_delete_document_with_rewrite_to_has_link_risk() {
        let tmp = synth_vault();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let index = crate::graph::build_index(root).unwrap();

        let op = MigrationOp {
            kind: "delete_document".into(),
            id: None,
            requires: vec![],
            fields: serde_json::json!({"path": "a.md", "rewrite_to": "b.md"}),
            footnote: None,
        };
        let expanded = expand(&op, &index).unwrap();
        assert_eq!(expanded.len(), 1);
        assert!(
            expanded[0].link_risk.is_some(),
            "delete_document with rewrite_to should have link_risk"
        );
    }

    #[test]
    fn dispatch_low_level_set_frontmatter_passes_through() {
        let tmp = synth_vault();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let index = crate::graph::build_index(root).unwrap();

        let op = MigrationOp {
            kind: "set_frontmatter".into(),
            id: None,
            requires: vec![],
            fields: serde_json::json!({"path": "a.md", "field": "title", "new_value": "Foo"}),
            footnote: None,
        };
        let expanded = expand(&op, &index).unwrap();
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0].operation, "set_frontmatter");
        assert_eq!(expanded[0].field.as_deref(), Some("title"));
    }

    #[test]
    fn dispatch_unknown_kind_returns_err() {
        let tmp = synth_vault();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let index = crate::graph::build_index(root).unwrap();

        let op = MigrationOp {
            kind: "no_such_kind".into(),
            id: None,
            requires: vec![],
            fields: serde_json::json!({}),
            footnote: None,
        };
        let result = expand(&op, &index);
        assert!(result.is_err());
    }
}
