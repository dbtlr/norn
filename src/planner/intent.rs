//! Intent op vocabulary + dispatch to per-kind expanders.
//!
//! Per-kind expanders land in submodules (Plan Tasks 4, 5). The dispatcher
//! in this file (Plan Task 6) will route high-level ops to expanders and
//! pass low-level ops through with planner-filled link_risk.

use serde::{Deserialize, Serialize};

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
