//! Typed refusal errors for `norn set`'s schema-aware validation and target /
//! assignment parsing (NRN-221).
//!
//! Mirrors the established `.code()` convention
//! ([`new::validate::PreflightError`](crate::new::validate::PreflightError),
//! [`edit::transform::EditError`](crate::edit::transform::EditError)): `Display`
//! preserves the EXACT prose the prior `anyhow::bail!`/`anyhow!()` call sites
//! produced (byte-identical CLI/stderr output), and `.code()` gives an MCP /
//! `--format json` consumer a stable, machine-branchable kebab code instead of
//! string-matching the message.
//!
//! `set`'s validation and synthesis functions keep returning `anyhow::Result`
//! (no signature churn) — each refusal site now constructs a `SetError`
//! variant and lets it propagate through the existing `?` chains, converted to
//! `anyhow::Error` at the point of return exactly as `PreflightError` and
//! `EditError` already are elsewhere. As long as nothing wraps it with
//! `.context(...)`, the concrete `SetError` survives at the top of the chain
//! for `downcast_ref` to recover.

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SetError {
    #[error("value '{value}' is not a valid datetime (expected YYYY-MM-DDTHH:MM[:SS])")]
    InvalidDatetime { value: String },

    #[error("value '{value}' is not a valid date (expected YYYY-MM-DD)")]
    InvalidDate { value: String },

    #[error("value '{value}' is not shape-valid as a wikilink (need non-empty stem inside [[…]])")]
    InvalidWikilink { value: String },

    #[error("unknown field_type: {field_type}")]
    UnknownFieldType { field_type: String },

    #[error("value '{value}' exceeds max_length {bound} for field type '{field_type}'")]
    ValueTooLong {
        value: String,
        bound: u32,
        field_type: String,
    },

    #[error("value '{value}' is not allowed for '{field}' (allowed: {allowed}); use --force to override")]
    ValueNotAllowed {
        field: String,
        value: String,
        allowed: String,
    },

    #[error("--field-json value is not valid JSON ({field}): {detail}")]
    FieldJsonInvalid { field: String, detail: String },

    #[error("--field-json value for '{field}' does not match schema type '{field_type}'")]
    FieldJsonTypeInvalid { field: String, field_type: String },

    #[error(
        "--field-json value for '{field}' is not allowed (allowed: {allowed}); use --force to override"
    )]
    FieldJsonNotAllowed { field: String, allowed: String },

    #[error("cannot remove required field '{field}'; use --force to override")]
    RequiredFieldRemoved { field: String },

    #[error(
        "doc not found: {target}\n  hint: '{target}' looks like a field assignment — did you forget the document argument? Usage: norn set <doc> [key=value ...]"
    )]
    TargetNotFoundHint { target: String },

    #[error("doc not found: {target}")]
    TargetNotFound { target: String },

    #[error("ambiguous doc target: '{target}' matches {count} docs: {candidates}")]
    TargetAmbiguous {
        target: String,
        count: usize,
        candidates: String,
    },

    #[error("expected KEY=VALUE, got: {raw}")]
    AssignmentMalformed { raw: String },

    #[error("KEY cannot be empty in: {raw}")]
    AssignmentKeyEmpty { raw: String },

    #[error("expected key=value, got '{raw}'")]
    PositionalAssignmentMalformed { raw: String },

    /// Cross-class conflict on the same key across `--field`/`--field-json`/
    /// `--push`/`--pop`/`--remove`. The full multi-line message (one line per
    /// conflicting key) is assembled by the caller since its shape is
    /// data-dependent; stored verbatim so `Display` stays byte-identical.
    #[error("{message}")]
    FieldConflict { message: String },

    #[error("--push on key '{key}' requires an array-typed value (current is scalar)")]
    PushOnScalar { key: String },

    #[error("frontmatter is not a top-level mapping")]
    FrontmatterNotMapping,

    #[error("frontmatter parse errors: {detail}")]
    FrontmatterParseFailed { detail: String },
}

impl SetError {
    /// The stable, machine-branchable kebab code for this refusal (NRN-221), so
    /// an MCP `vault.set` consumer branches on the code rather than string-
    /// matching the prose message. Reuses `frontmatter-parse-failed` — the same
    /// code [`standards::apply::ApplyError::FrontmatterParseFailed`] uses for
    /// the identical semantic (a document's on-disk frontmatter can't be
    /// parsed) — rather than minting a synonym.
    pub fn code(&self) -> &'static str {
        match self {
            SetError::InvalidDatetime { .. }
            | SetError::InvalidDate { .. }
            | SetError::InvalidWikilink { .. }
            | SetError::UnknownFieldType { .. }
            | SetError::FieldJsonTypeInvalid { .. } => "field-type-invalid",
            SetError::ValueTooLong { .. } => "value-too-long",
            SetError::ValueNotAllowed { .. } | SetError::FieldJsonNotAllowed { .. } => {
                "value-not-allowed"
            }
            SetError::FieldJsonInvalid { .. } => "field-json-invalid",
            SetError::RequiredFieldRemoved { .. } => "required-field-removed",
            SetError::TargetNotFoundHint { .. } | SetError::TargetNotFound { .. } => {
                "target-not-found"
            }
            SetError::TargetAmbiguous { .. } => "target-ambiguous",
            SetError::AssignmentMalformed { .. }
            | SetError::AssignmentKeyEmpty { .. }
            | SetError::PositionalAssignmentMalformed { .. } => "assignment-malformed",
            SetError::FieldConflict { .. } => "field-conflict",
            SetError::PushOnScalar { .. } => "push-on-scalar",
            SetError::FrontmatterNotMapping => "frontmatter-not-mapping",
            SetError::FrontmatterParseFailed { .. } => "frontmatter-parse-failed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_has_a_kebab_code() {
        let cases: Vec<SetError> = vec![
            SetError::InvalidDatetime { value: "x".into() },
            SetError::InvalidDate { value: "x".into() },
            SetError::InvalidWikilink { value: "x".into() },
            SetError::UnknownFieldType {
                field_type: "x".into(),
            },
            SetError::ValueTooLong {
                value: "x".into(),
                bound: 1,
                field_type: "string".into(),
            },
            SetError::ValueNotAllowed {
                field: "x".into(),
                value: "x".into(),
                allowed: "a, b".into(),
            },
            SetError::FieldJsonInvalid {
                field: "x".into(),
                detail: "x".into(),
            },
            SetError::FieldJsonTypeInvalid {
                field: "x".into(),
                field_type: "string".into(),
            },
            SetError::FieldJsonNotAllowed {
                field: "x".into(),
                allowed: "a, b".into(),
            },
            SetError::RequiredFieldRemoved { field: "x".into() },
            SetError::TargetNotFoundHint { target: "x".into() },
            SetError::TargetNotFound { target: "x".into() },
            SetError::TargetAmbiguous {
                target: "x".into(),
                count: 2,
                candidates: "a.md, b.md".into(),
            },
            SetError::AssignmentMalformed { raw: "x".into() },
            SetError::AssignmentKeyEmpty { raw: "x".into() },
            SetError::PositionalAssignmentMalformed { raw: "x".into() },
            SetError::FieldConflict {
                message: "x".into(),
            },
            SetError::PushOnScalar { key: "x".into() },
            SetError::FrontmatterNotMapping,
            SetError::FrontmatterParseFailed { detail: "x".into() },
        ];
        for case in cases {
            let code = case.code();
            assert!(
                !code.is_empty() && code.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "code {code:?} for {case:?} is not kebab-case"
            );
        }
    }

    #[test]
    fn field_type_family_shares_one_code() {
        assert_eq!(
            SetError::InvalidDatetime { value: "x".into() }.code(),
            "field-type-invalid"
        );
        assert_eq!(
            SetError::InvalidDate { value: "x".into() }.code(),
            "field-type-invalid"
        );
        assert_eq!(
            SetError::InvalidWikilink { value: "x".into() }.code(),
            "field-type-invalid"
        );
        assert_eq!(
            SetError::UnknownFieldType {
                field_type: "x".into()
            }
            .code(),
            "field-type-invalid"
        );
        assert_eq!(
            SetError::FieldJsonTypeInvalid {
                field: "x".into(),
                field_type: "y".into()
            }
            .code(),
            "field-type-invalid"
        );
    }

    #[test]
    fn display_preserves_exact_prose() {
        assert_eq!(
            SetError::InvalidDatetime {
                value: "2020-13-40".into()
            }
            .to_string(),
            "value '2020-13-40' is not a valid datetime (expected YYYY-MM-DDTHH:MM[:SS])"
        );
        assert_eq!(
            SetError::RequiredFieldRemoved {
                field: "status".into()
            }
            .to_string(),
            "cannot remove required field 'status'; use --force to override"
        );
        assert_eq!(
            SetError::TargetNotFoundHint {
                target: "status=done".into()
            }
            .to_string(),
            "doc not found: status=done\n  hint: 'status=done' looks like a field assignment — did you forget the document argument? Usage: norn set <doc> [key=value ...]"
        );
        assert_eq!(
            SetError::TargetAmbiguous {
                target: "hub".into(),
                count: 2,
                candidates: "a.md, b.md".into(),
            }
            .to_string(),
            "ambiguous doc target: 'hub' matches 2 docs: a.md, b.md"
        );
    }
}
