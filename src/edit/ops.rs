//! The `norn edit` op set — a content-anchored tagged union, deserialized from
//! the `edits` array carried identically by `norn edit` (CLI) and `vault.edit`
//! (MCP). Internally tagged on `op`; `deny_unknown_fields` is intentionally
//! omitted (serde forbids it on internally-tagged enums).

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum EditOp {
    /// Replace literal `old` with `new`. Unique-match-or-refuse unless
    /// `replace_all` is set.
    StrReplace {
        old: String,
        new: String,
        #[serde(default)]
        replace_all: bool,
    },
    /// Replace a section's body (heading line kept), by exact heading text.
    ReplaceSection { heading: String, content: String },
    /// Append content to the end of a section's body.
    AppendToSection { heading: String, content: String },
    /// Remove a heading line and its body.
    DeleteSection { heading: String },
    /// Insert content immediately before a heading line.
    InsertBeforeHeading { heading: String, content: String },
    /// Insert content immediately after a heading line (before the body).
    InsertAfterHeading { heading: String, content: String },
}

impl EditOp {
    /// Stable snake_case discriminant, used in reports and error messages.
    pub fn kind(&self) -> &'static str {
        match self {
            EditOp::StrReplace { .. } => "str_replace",
            EditOp::ReplaceSection { .. } => "replace_section",
            EditOp::AppendToSection { .. } => "append_to_section",
            EditOp::DeleteSection { .. } => "delete_section",
            EditOp::InsertBeforeHeading { .. } => "insert_before_heading",
            EditOp::InsertAfterHeading { .. } => "insert_after_heading",
        }
    }

    /// Human-readable anchor summary for the report `edits` array.
    pub fn anchor_desc(&self) -> String {
        match self {
            EditOp::StrReplace { old, .. } => format!("old={:?}", truncate(old)),
            EditOp::ReplaceSection { heading, .. }
            | EditOp::AppendToSection { heading, .. }
            | EditOp::DeleteSection { heading }
            | EditOp::InsertBeforeHeading { heading, .. }
            | EditOp::InsertAfterHeading { heading, .. } => format!("heading={heading:?}"),
        }
    }
}

fn truncate(s: &str) -> String {
    const MAX: usize = 40;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        let head: String = s.chars().take(MAX).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_str_replace_with_default_replace_all() {
        let op: EditOp =
            serde_json::from_str(r#"{"op":"str_replace","old":"a","new":"b"}"#).unwrap();
        assert_eq!(
            op,
            EditOp::StrReplace {
                old: "a".into(),
                new: "b".into(),
                replace_all: false
            }
        );
        assert_eq!(op.kind(), "str_replace");
    }

    #[test]
    fn deserializes_structural_ops() {
        let op: EditOp =
            serde_json::from_str(r#"{"op":"append_to_section","heading":"Tasks","content":"- x"}"#)
                .unwrap();
        assert_eq!(op.kind(), "append_to_section");
        assert_eq!(op.anchor_desc(), r#"heading="Tasks""#);
    }

    #[test]
    fn rejects_unknown_op() {
        let err = serde_json::from_str::<EditOp>(r#"{"op":"nope"}"#);
        assert!(err.is_err());
    }
}
