//! The pure body-transform for `norn edit`: `(old_body, [EditOp]) -> new_body`.
//! Ops apply sequentially, each against the result of the prior. Any anchor
//! failure aborts the whole batch (atomic refuse) via `EditError`. This module
//! is the novel core and carries the bulk of the test weight; everything
//! downstream (lock, audit, report) is reused from `set`.

use crate::edit::ops::EditOp;

/// Per-op descriptor for the success report `edits` array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditDescriptor {
    pub op: String,
    pub anchor_desc: String,
    /// Match count for `str_replace`; `None` for structural ops.
    pub occurrences: Option<usize>,
}

#[derive(Debug)]
pub struct EditTransform {
    pub new_body: String,
    pub descriptors: Vec<EditDescriptor>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EditError {
    #[error("edit {index} ({kind}): empty anchor is not allowed")]
    InvalidOp { index: usize, kind: &'static str },
    #[error("edit {index} ({kind}): string not found: {anchor}")]
    StrNotFound {
        index: usize,
        kind: &'static str,
        anchor: String,
    },
    #[error("edit {index} ({kind}): string matched {count} times, expected exactly 1 (set replace_all to replace every occurrence): {anchor}")]
    StrAmbiguous {
        index: usize,
        kind: &'static str,
        anchor: String,
        count: usize,
    },
    #[error("edit {index} ({kind}): heading not found: {heading:?}")]
    HeadingNotFound {
        index: usize,
        kind: &'static str,
        heading: String,
    },
    #[error("edit {index} ({kind}): {count} headings named {heading:?}; heading must be unambiguous")]
    HeadingAmbiguous {
        index: usize,
        kind: &'static str,
        heading: String,
        count: usize,
    },
}

/// Apply `ops` to `old_body` sequentially. Returns the new body plus per-op
/// descriptors, or the first `EditError` (nothing is applied on error — the
/// caller never writes a partial result).
pub fn apply_edits(old_body: &str, ops: &[EditOp]) -> Result<EditTransform, EditError> {
    let mut body = old_body.to_string();
    let mut descriptors = Vec::with_capacity(ops.len());
    for (index, op) in ops.iter().enumerate() {
        let occurrences = apply_one(&mut body, op, index)?;
        descriptors.push(EditDescriptor {
            op: op.kind().to_string(),
            anchor_desc: op.anchor_desc(),
            occurrences,
        });
    }
    Ok(EditTransform {
        new_body: body,
        descriptors,
    })
}

/// Apply a single op in place. Returns the str_replace match count (else None).
fn apply_one(body: &mut String, op: &EditOp, index: usize) -> Result<Option<usize>, EditError> {
    match op {
        EditOp::StrReplace {
            old,
            new,
            replace_all,
        } => {
            if old.is_empty() {
                return Err(EditError::InvalidOp {
                    index,
                    kind: op.kind(),
                });
            }
            let count = body.matches(old.as_str()).count();
            if count == 0 {
                return Err(EditError::StrNotFound {
                    index,
                    kind: op.kind(),
                    anchor: op.anchor_desc(),
                });
            }
            if !replace_all && count > 1 {
                return Err(EditError::StrAmbiguous {
                    index,
                    kind: op.kind(),
                    anchor: op.anchor_desc(),
                    count,
                });
            }
            *body = if *replace_all {
                body.replace(old.as_str(), new)
            } else {
                body.replacen(old.as_str(), new, 1)
            };
            Ok(Some(count))
        }
        // Structural ops land in Tasks 3–5.
        _ => unimplemented!("structural ops"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn str_replace(old: &str, new: &str, all: bool) -> EditOp {
        EditOp::StrReplace {
            old: old.into(),
            new: new.into(),
            replace_all: all,
        }
    }

    #[test]
    fn str_replace_unique_succeeds() {
        let out = apply_edits("hello world", &[str_replace("world", "norn", false)]).unwrap();
        assert_eq!(out.new_body, "hello norn");
        assert_eq!(out.descriptors[0].occurrences, Some(1));
    }

    #[test]
    fn str_replace_not_found_refuses() {
        let err = apply_edits("hello", &[str_replace("xyz", "q", false)]).unwrap_err();
        assert!(matches!(err, EditError::StrNotFound { index: 0, .. }));
    }

    #[test]
    fn str_replace_ambiguous_refuses() {
        let err = apply_edits("a a a", &[str_replace("a", "b", false)]).unwrap_err();
        assert_eq!(
            err,
            EditError::StrAmbiguous {
                index: 0,
                kind: "str_replace",
                anchor: r#"old="a""#.into(),
                count: 3
            }
        );
    }

    #[test]
    fn str_replace_all_replaces_every_occurrence() {
        let out = apply_edits("a a a", &[str_replace("a", "b", true)]).unwrap();
        assert_eq!(out.new_body, "b b b");
        assert_eq!(out.descriptors[0].occurrences, Some(3));
    }

    #[test]
    fn str_replace_empty_old_refuses() {
        let err = apply_edits("x", &[str_replace("", "y", false)]).unwrap_err();
        assert!(matches!(err, EditError::InvalidOp { index: 0, .. }));
    }
}
