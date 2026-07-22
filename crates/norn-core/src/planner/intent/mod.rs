//! Intent op vocabulary + dispatch to per-kind expanders.
//!
//! Per-kind expanders land in submodules (Plan Tasks 4, 5). The dispatcher
//! in this file (Plan Task 6) routes high-level ops to expanders and
//! passes low-level ops through with planner-filled link_risk.

use crate::domain::GraphIndex;
use crate::standards::{classify_link_risk, ApplyOp};
use anyhow::Result;
use camino::Utf8PathBuf;
use norn_wire::{ChangeOp, EditOp, MigrationOp, MoveDocumentFields, TypedOp, TypedOpError};
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
/// `Vec<ApplyOp>` ready for the applier.
///
/// The op is first resolved to the typed [`TypedOp`] vocabulary
/// (`TypedOp::try_from`), so no untyped `fields` indexing flows into this
/// executor seam. A conversion failure — an **unknown kind** or a structural op
/// missing a required field — surfaces as the same `anyhow` error the executor
/// previously produced (the message strings are preserved by
/// [`TypedOpError`](norn_wire::TypedOpError)), so the plan-parses-but-refused-at-
/// execute contract is unchanged. The typed error rides through `anyhow`
/// unflattened (`anyhow::Error::new`, not `anyhow!("{e}")`) so
/// [`from_anyhow`](crate::apply::envelope::from_anyhow) can downcast it to a real
/// refusal code (`unknown-operation-kind` / `malformed-plan`) rather than the
/// misleading `internal-error`.
///
/// - **High-level kinds** (`move_folder`, `rewrite_wikilink`): dispatch to
///   the corresponding expander.
/// - **Low-level move/delete** (`move_document`, `delete_document`): pass
///   through with `link_risk` populated by `classify_link_risk`.
/// - **Change ops** (`set_frontmatter`, `add_frontmatter`, `remove_frontmatter`,
///   `rewrite_link`, `replace_body`, `create_document`): deserialize the typed
///   op's `fields` into a `ApplyOp` (a `norn-core` model `norn-wire` may not
///   name — hence the payload rides typed-through as a JSON object).
/// - **Edit ops**: carry the `EditOp` payload for apply-time application.
pub(crate) fn expand(op: &MigrationOp, index: &GraphIndex) -> Result<Vec<ApplyOp>> {
    match TypedOp::try_from(op).map_err(anyhow::Error::new)? {
        TypedOp::MoveFolder(f) => move_folder::expand_move_folder(
            &move_folder::MoveFolderOp {
                src: f.src,
                dst: f.dst,
                parents: f.parents,
            },
            index,
        ),

        TypedOp::RewriteWikilink(f) => rewrite_wikilink::expand_rewrite_wikilink(
            &rewrite_wikilink::RewriteWikilinkOp {
                old: f.old,
                new: f.new,
            },
            index,
        ),

        TypedOp::MoveDocument(MoveDocumentFields {
            src,
            dst,
            parents,
            force,
            no_link_rewrite,
        }) => {
            let old_path: Utf8PathBuf = src.clone().into();
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

            let change = ApplyOp {
                change_id: format!("move-{src}"),
                path: old_path,
                document_hash: String::new(),
                finding_code: None,
                finding_rule: None,
                repair_rule: None,
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

        TypedOp::DeleteDocument(f) => {
            let doc_path: Utf8PathBuf = f.path.clone().into();

            // Only populate link_risk when rewrite_to is present.
            let link_risk = f.rewrite_to.as_deref().map(|rewrite_to| {
                let rewrite_path: Utf8PathBuf = rewrite_to.into();
                classify_link_risk(&doc_path, &rewrite_path, &index.documents, &index.files)
            });

            let change = ApplyOp {
                change_id: format!("delete-{}", f.path),
                path: doc_path,
                document_hash: String::new(),
                finding_code: None,
                finding_rule: None,
                repair_rule: None,
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

        TypedOp::Change(ChangeOp { kind, fields }) => {
            // `fields.operation` is the value that actually drives write dispatch
            // (executor content-class routing). If an authored plan supplies one
            // that disagrees with the op's `kind`, the reviewed `kind` is a lie:
            // the plan would execute a different operation than it declares.
            // Refuse rather than silently reinterpret. A repair-sourced op
            // (operation == kind) and an authored op omitting `operation` are
            // both unaffected — the latter fills from `kind` below. A wrong-TYPED
            // `operation` (e.g. `5`) never reaches here: it fails the strict
            // `ChangeFields` decode as `MalformedFields`.
            if let Some(operation) = &fields.operation {
                if operation != &kind {
                    return Err(anyhow::Error::new(TypedOpError::OperationKindMismatch {
                        kind: kind.clone(),
                        operation: operation.clone(),
                    }));
                }
            }
            // Adapt the wire-typed `ChangeFields` into `norn-core`'s `ApplyOp`.
            // Finding linkage rides through as `Option<String>` — a repair-sourced
            // op carries the real codes, an authored op leaves them `None` (ADR
            // 0022); no `operator-request` sentinel is fabricated (the interior op
            // is optional end-to-end). The report echoes linkage from the wire op,
            // not this interior view. `link_risk` / `warnings` are planner-computed
            // and never carried by a change op.
            let change = ApplyOp {
                change_id: fields
                    .change_id
                    .clone()
                    .unwrap_or_else(|| format!("{kind}-{}", fields.path)),
                path: fields.path.clone().into(),
                document_hash: fields.document_hash.clone().unwrap_or_default(),
                finding_code: fields.finding_code.clone(),
                finding_rule: fields.finding_rule.clone(),
                repair_rule: fields.repair_rule.clone(),
                operation: kind,
                field: fields.field.clone(),
                expected_old_value: fields.expected_old_value.clone(),
                new_value: fields.new_value.clone(),
                destination: fields.destination.clone().map(Into::into),
                link_risk: None,
                warnings: Vec::new(),
                force: fields.force,
                parents: fields.parents,
            };
            Ok(vec![change])
        }

        TypedOp::Edit(EditOp {
            kind,
            id,
            path,
            fields,
        }) => {
            // Section/body edit ops (NRN-98 / H1). Reassemble the wire-typed
            // `EditFields` into the `op`-tagged JSON the transform engine
            // deserializes into its own `EditOp` at apply time (under whole-doc
            // CAS), carried in `new_value`. `path`/`document_hash` ride along
            // (ignored by that deserializer) so for a well-formed op the
            // reconstructed payload — and the internal `change_id` span-correlation
            // digest derived from it — matches the on-disk op. Unknown extra keys
            // and a `document_hash: null` are dropped by the typed decode and so
            // shift the digest; `change_id` never crosses a wire, so only telemetry
            // correlation is affected. `Value::Object` is a `BTreeMap`, so member
            // insertion order does not affect the serialized bytes.
            let mut payload = serde_json::Map::new();
            payload.insert("path".into(), serde_json::Value::String(path.clone()));
            if let Some(dh) = &fields.document_hash {
                payload.insert(
                    "document_hash".into(),
                    serde_json::Value::String(dh.clone()),
                );
            }
            if let Some(v) = &fields.old {
                payload.insert("old".into(), serde_json::Value::String(v.clone()));
            }
            if let Some(v) = &fields.new {
                payload.insert("new".into(), serde_json::Value::String(v.clone()));
            }
            if let Some(v) = fields.replace_all {
                payload.insert("replace_all".into(), serde_json::Value::Bool(v));
            }
            if let Some(v) = &fields.heading {
                payload.insert("heading".into(), serde_json::Value::String(v.clone()));
            }
            if let Some(v) = &fields.content {
                payload.insert("content".into(), serde_json::Value::String(v.clone()));
            }
            payload.insert("op".into(), serde_json::Value::String(kind.clone()));

            // change_id must be unique per op: two edits of the same kind on the
            // same document would otherwise collide and clobber each other's
            // telemetry span. Prefer the plan-supplied `op.id`; else discriminate
            // by a hash of the edit payload.
            let change_id = id.unwrap_or_else(|| {
                let digest = blake3::hash(
                    serde_json::Value::Object(payload.clone())
                        .to_string()
                        .as_bytes(),
                )
                .to_hex();
                format!("{kind}-{path}-{}", &digest[..8])
            });

            let change = ApplyOp {
                change_id,
                path: path.into(),
                document_hash: fields.document_hash.clone().unwrap_or_default(),
                finding_code: None,
                finding_rule: None,
                repair_rule: None,
                operation: kind,
                field: None,
                expected_old_value: None,
                new_value: Some(serde_json::Value::Object(payload)),
                destination: None,
                link_risk: None,
                warnings: Vec::new(),
                force: false,
                parents: false,
            };
            Ok(vec![change])
        }
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
    use norn_wire::MigrationOp;
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
    fn dispatch_change_operation_matching_kind_passes() {
        let tmp = synth_vault();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let index = crate::graph::build_index(root).unwrap();

        let op = MigrationOp {
            kind: "set_frontmatter".into(),
            id: None,
            requires: vec![],
            fields: serde_json::json!({
                "path": "a.md",
                "field": "title",
                "new_value": "Foo",
                "operation": "set_frontmatter",
            }),
            footnote: None,
        };
        let expanded = expand(&op, &index).unwrap();
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0].operation, "set_frontmatter");
    }

    #[test]
    fn dispatch_change_operation_mismatching_kind_refuses() {
        let tmp = synth_vault();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let index = crate::graph::build_index(root).unwrap();

        let op = MigrationOp {
            kind: "set_frontmatter".into(),
            id: None,
            requires: vec![],
            fields: serde_json::json!({
                "path": "a.md",
                "field": "title",
                "new_value": "Foo",
                "operation": "remove_frontmatter",
            }),
            footnote: None,
        };
        let err = expand(&op, &index).unwrap_err();
        let typed = err
            .downcast_ref::<TypedOpError>()
            .expect("mismatch must surface the typed error, not a flattened anyhow");
        assert_eq!(
            *typed,
            TypedOpError::OperationKindMismatch {
                kind: "set_frontmatter".into(),
                operation: "remove_frontmatter".into(),
            }
        );
        assert_eq!(
            typed.to_string(),
            "op.fields.operation 'remove_frontmatter' conflicts with op.kind 'set_frontmatter'"
        );
    }

    #[test]
    fn dispatch_change_operation_absent_fills_from_kind() {
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
        assert_eq!(
            expanded[0].operation, "set_frontmatter",
            "an absent operation fills from kind"
        );
    }

    #[test]
    fn dispatch_change_non_string_operation_is_malformed_plan() {
        let tmp = synth_vault();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let index = crate::graph::build_index(root).unwrap();

        // A non-string `operation` slips the mismatch guard (`as_str()` -> None)
        // and the `entry().or_insert` no-op (key present), then fails the decode
        // into the change model. It must surface as a typed MalformedFields, not a
        // flattened serde anyhow (which would code `internal-error`).
        let op = MigrationOp {
            kind: "set_frontmatter".into(),
            id: None,
            requires: vec![],
            fields: serde_json::json!({
                "path": "a.md",
                "field": "title",
                "new_value": "Foo",
                "operation": 5,
            }),
            footnote: None,
        };
        let err = expand(&op, &index).unwrap_err();
        let typed = err
            .downcast_ref::<TypedOpError>()
            .expect("a wrong-typed field must surface the typed error");
        assert!(
            matches!(typed, TypedOpError::MalformedFields { kind, .. } if kind == "set_frontmatter"),
            "expected MalformedFields for set_frontmatter, got {typed:?}"
        );
    }

    #[test]
    fn dispatch_change_wrong_typed_field_is_malformed_plan() {
        let tmp = synth_vault();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let index = crate::graph::build_index(root).unwrap();

        // `force` is a bool in the change model; a string value fails the decode.
        let op = MigrationOp {
            kind: "set_frontmatter".into(),
            id: None,
            requires: vec![],
            fields: serde_json::json!({
                "path": "a.md",
                "field": "title",
                "new_value": "Foo",
                "force": "notabool",
            }),
            footnote: None,
        };
        let err = expand(&op, &index).unwrap_err();
        let typed = err
            .downcast_ref::<TypedOpError>()
            .expect("a wrong-typed field must surface the typed error");
        assert!(
            matches!(typed, TypedOpError::MalformedFields { .. }),
            "expected MalformedFields, got {typed:?}"
        );
    }

    #[test]
    fn dispatch_edit_op_reconstructs_op_tagged_payload() {
        let tmp = synth_vault();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let index = crate::graph::build_index(root).unwrap();

        let op = MigrationOp {
            kind: "str_replace".into(),
            id: None,
            requires: vec![],
            fields: serde_json::json!({
                "path": "a.md",
                "old": "foo",
                "new": "bar",
                "document_hash": "cafe"
            }),
            footnote: None,
        };
        let expanded = expand(&op, &index).unwrap();
        assert_eq!(expanded.len(), 1);
        let change = &expanded[0];
        assert_eq!(change.operation, "str_replace");
        assert_eq!(change.document_hash, "cafe");
        // new_value carries the `op` discriminant plus the body, so the apply-time
        // `EditOp` deserializer sees a valid op.
        let nv = change.new_value.as_ref().unwrap();
        assert_eq!(nv["op"], "str_replace");
        assert_eq!(nv["old"], "foo");
        assert_eq!(nv["new"], "bar");
        // path/document_hash ride along (ignored by the EditOp deserializer) so the
        // reconstructed payload matches the on-disk op the change_id derives from.
        assert_eq!(nv["path"], "a.md");
        assert_eq!(nv["document_hash"], "cafe");
    }

    #[test]
    fn dispatch_change_op_echoes_finding_linkage_and_defaults_when_absent() {
        let tmp = synth_vault();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let index = crate::graph::build_index(root).unwrap();

        // A repair-sourced op carries real linkage → ApplyOp echoes it.
        let repair_op = MigrationOp {
            kind: "rewrite_link".into(),
            id: None,
            requires: vec![],
            fields: serde_json::json!({
                "path": "a.md",
                "finding_code": "link-target-missing",
                "repair_rule": "closest-match"
            }),
            footnote: None,
        };
        let change = expand(&repair_op, &index).unwrap().pop().unwrap();
        assert_eq!(change.finding_code.as_deref(), Some("link-target-missing"));
        assert_eq!(change.repair_rule.as_deref(), Some("closest-match"));

        // An authored op omits linkage → the interior op leaves it `None` (no
        // `operator-request` sentinel; linkage is optional end-to-end).
        let authored_op = MigrationOp {
            kind: "set_frontmatter".into(),
            id: None,
            requires: vec![],
            fields: serde_json::json!({"path": "a.md", "field": "title", "new_value": "T"}),
            footnote: None,
        };
        let change = expand(&authored_op, &index).unwrap().pop().unwrap();
        assert_eq!(change.finding_code, None);
        assert_eq!(change.repair_rule, None);
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
