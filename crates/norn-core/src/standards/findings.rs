//! The validate finding model: one [`Finding`] per rule/link/graph violation.
//!
//! Ported from the donor `src/standards/findings.rs` (ADR 0018). A finding
//! carries a stable `code`, a [`Severity`], the offending document `path`, a
//! human `message`, and a typed [`FindingBody`] variant with the structured
//! detail (`--format json`/`jsonl` serialize the whole struct). The constructors
//! are the single source of each finding's code + message text; the validate
//! `checks` call them so the wording cannot drift.

use crate::domain::{Diagnostic, Link, Severity};
use camino::Utf8PathBuf;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub code: String,
    pub severity: Severity,
    pub path: Utf8PathBuf,
    pub message: String,
    #[serde(flatten)]
    pub body: FindingBody,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum FindingBody {
    GraphDiagnostic {
        diagnostic: Diagnostic,
    },
    LinkIssue {
        link: Link,
    },
    RequiredFrontmatterMissing {
        rule: Option<String>,
        field: String,
    },
    DisallowedValue {
        rule: Option<String>,
        field: String,
        actual_value: Value,
        allowed_values: Vec<Value>,
    },
    InvalidFieldType {
        rule: Option<String>,
        field: String,
        actual_value: Value,
        expected_type: String,
    },
    ExceedsMaxLength {
        rule: Option<String>,
        field: String,
        actual_value: Value,
        max_length: u32,
        actual_length: usize,
    },
    ForbiddenField {
        rule: Option<String>,
        field: String,
        actual_value: Value,
    },
    DocumentMisrouted {
        rule: Option<String>,
        allowed_paths: Vec<String>,
    },
    ReferenceType {
        rule: Option<String>,
        field: String,
        reference: String,
        target: Utf8PathBuf,
        actual_type: String,
        allowed_types: Vec<String>,
    },
    AliasMalformed {
        field: String,
        invalid_entries: Vec<Value>,
    },
    AliasShadowedByStem {
        alias_value: String,
        shadowing_doc_path: Utf8PathBuf,
    },
    AliasDuplicateAcrossDocs {
        alias_value: String,
        peer_doc_paths: Vec<Utf8PathBuf>,
    },
    NonportableFilename {
        issues: Vec<String>,
    },
}

impl Finding {
    pub fn from_graph_diagnostic(path: Utf8PathBuf, diagnostic: Diagnostic) -> Self {
        Self {
            code: diagnostic.code.clone(),
            severity: diagnostic.severity,
            message: diagnostic.message.clone(),
            path,
            body: FindingBody::GraphDiagnostic { diagnostic },
        }
    }

    /// Construct a link-shaped finding by dispatching on the link's status
    /// and unresolved_reason. Returns one of:
    /// - `link-target-missing` for Unresolved + TargetMissing
    /// - `link-anchor-missing` for Unresolved + AnchorMissing
    /// - `link-block-missing` for Unresolved + BlockRefMissing
    /// - `link-ambiguous` for Ambiguous status
    ///
    /// Falls back to `link-target-missing` for Unresolved with no reason set;
    /// emitter is expected to populate reason but we don't panic if absent.
    pub fn from_link(path: Utf8PathBuf, link: Link) -> Self {
        use crate::domain::{LinkStatus, UnresolvedReason};

        let (code, message) = match (&link.status, &link.unresolved_reason) {
            (LinkStatus::Ambiguous, _) => (
                "link-ambiguous",
                format!("ambiguous link target: {}", link.target),
            ),
            (LinkStatus::Unresolved, Some(UnresolvedReason::AnchorMissing)) => (
                "link-anchor-missing",
                format!(
                    "link anchor not found in target: {}#{}",
                    link.target,
                    link.anchor.as_deref().unwrap_or("")
                ),
            ),
            (LinkStatus::Unresolved, Some(UnresolvedReason::BlockRefMissing)) => (
                "link-block-missing",
                format!(
                    "link block-ref not found in target: {}^{}",
                    link.target,
                    link.block_ref.as_deref().unwrap_or("")
                ),
            ),
            _ => (
                "link-target-missing",
                format!("link target not found: {}", link.target),
            ),
        };

        Self {
            code: code.to_string(),
            severity: Severity::Warning,
            path,
            message,
            body: FindingBody::LinkIssue { link },
        }
    }

    pub fn frontmatter_required_missing(
        path: Utf8PathBuf,
        rule: Option<String>,
        field: String,
    ) -> Self {
        let message = format!("required frontmatter field is missing: {field}");
        Self {
            code: "frontmatter-required-field-missing".to_string(),
            severity: Severity::Warning,
            path,
            message,
            body: FindingBody::RequiredFrontmatterMissing { rule, field },
        }
    }

    pub fn frontmatter_disallowed_value(
        path: Utf8PathBuf,
        rule: Option<String>,
        field: String,
        actual_value: Value,
        allowed_values: Vec<Value>,
    ) -> Self {
        let message = format!("frontmatter field has a disallowed value: {field}");
        Self {
            code: "value-not-allowed".to_string(),
            severity: Severity::Warning,
            path,
            message,
            body: FindingBody::DisallowedValue {
                rule,
                field,
                actual_value,
                allowed_values,
            },
        }
    }

    /// A frontmatter wikilink resolves to a document whose `type` is outside
    /// the field's `field_references.target_type` set.
    #[allow(clippy::too_many_arguments)]
    pub fn frontmatter_reference_type(
        path: Utf8PathBuf,
        rule: Option<String>,
        field: String,
        reference: String,
        target: Utf8PathBuf,
        actual_type: String,
        allowed_types: Vec<String>,
    ) -> Self {
        let message = format!(
            "frontmatter field references a document of a disallowed type: {field} → {target} (type: {actual_type})"
        );
        Self {
            code: "frontmatter-reference-type".to_string(),
            severity: Severity::Warning,
            path,
            message,
            body: FindingBody::ReferenceType {
                rule,
                field,
                reference,
                target,
                actual_type,
                allowed_types,
            },
        }
    }

    pub fn frontmatter_invalid_type(
        path: Utf8PathBuf,
        rule: Option<String>,
        field: String,
        actual_value: Value,
        expected_type: String,
    ) -> Self {
        let message =
            format!("frontmatter field has invalid type: {field}; expected {expected_type}");
        Self {
            code: "field-type-invalid".to_string(),
            severity: Severity::Warning,
            path,
            message,
            body: FindingBody::InvalidFieldType {
                rule,
                field,
                actual_value,
                expected_type,
            },
        }
    }

    /// A `string`/`list_of_strings` value matches its declared type's shape but
    /// exceeds the effective `max_length` bound (declared, or the type default).
    pub fn frontmatter_exceeds_max_length(
        path: Utf8PathBuf,
        rule: Option<String>,
        field: String,
        actual_value: Value,
        max_length: u32,
        actual_length: usize,
    ) -> Self {
        let message = format!(
            "frontmatter field exceeds max length: {field} ({actual_length} > {max_length})"
        );
        Self {
            code: "frontmatter-exceeds-max-length".to_string(),
            severity: Severity::Warning,
            path,
            message,
            body: FindingBody::ExceedsMaxLength {
                rule,
                field,
                actual_value,
                max_length,
                actual_length,
            },
        }
    }

    pub fn frontmatter_forbidden_field(
        path: Utf8PathBuf,
        rule: Option<String>,
        field: String,
        actual_value: Value,
    ) -> Self {
        let message = format!("frontmatter field is forbidden: {field}");
        Self {
            code: "frontmatter-forbidden-field".to_string(),
            severity: Severity::Warning,
            path,
            message,
            body: FindingBody::ForbiddenField {
                rule,
                field,
                actual_value,
            },
        }
    }

    pub fn document_misrouted(
        path: Utf8PathBuf,
        rule: Option<String>,
        allowed_paths: Vec<String>,
    ) -> Self {
        Self {
            code: "document-misrouted".to_string(),
            severity: Severity::Warning,
            path,
            message: "document path is outside allowed rule locations".to_string(),
            body: FindingBody::DocumentMisrouted {
                rule,
                allowed_paths,
            },
        }
    }

    pub fn frontmatter_alias_malformed(
        path: Utf8PathBuf,
        field: String,
        invalid_entries: Vec<Value>,
    ) -> Self {
        let message = format!(
            "alias field '{field}' contains {} non-scalar value(s); entries skipped from resolution",
            invalid_entries.len()
        );
        Self {
            code: "frontmatter-alias-malformed".to_string(),
            severity: Severity::Warning,
            path,
            message,
            body: FindingBody::AliasMalformed {
                field,
                invalid_entries,
            },
        }
    }

    pub fn frontmatter_alias_shadowed_by_stem(
        path: Utf8PathBuf,
        alias_value: String,
        shadowing_doc_path: Utf8PathBuf,
    ) -> Self {
        let message = if shadowing_doc_path == path {
            format!(
                "alias '{alias_value}' is shadowed by this doc's own stem; alias is dead in fallback resolution"
            )
        } else {
            format!(
                "alias '{alias_value}' is shadowed by stem of {shadowing_doc_path}; alias is dead in fallback resolution"
            )
        };
        Self {
            code: "frontmatter-alias-shadowed-by-stem".to_string(),
            severity: Severity::Warning,
            path,
            message,
            body: FindingBody::AliasShadowedByStem {
                alias_value,
                shadowing_doc_path,
            },
        }
    }

    pub fn frontmatter_alias_duplicate_across_docs(
        path: Utf8PathBuf,
        alias_value: String,
        peer_doc_paths: Vec<Utf8PathBuf>,
    ) -> Self {
        let message = format!(
            "alias '{alias_value}' is also claimed by {} other doc(s); resolution will be ambiguous",
            peer_doc_paths.len()
        );
        Self {
            code: "frontmatter-alias-duplicate-across-docs".to_string(),
            severity: Severity::Warning,
            path,
            message,
            body: FindingBody::AliasDuplicateAcrossDocs {
                alias_value,
                peer_doc_paths,
            },
        }
    }

    /// (NRN-368) A document path contains a cross-platform-illegal character
    /// (NTFS-forbidden; Obsidian refuses them; macOS half-breaks colons) or a
    /// Windows-illegal segment shape (leading/trailing space, trailing dot) in
    /// any path segment. `issues` names every violation across the whole path
    /// — one finding per document even when several segments/classes are
    /// implicated (norn diagnoses; it never renames — that would be a move
    /// cascade, out of repair's scope).
    pub fn nonportable_filename(path: Utf8PathBuf, issues: Vec<String>) -> Self {
        let message = format!(
            "document path is not portable across platforms: {}",
            issues.join("; ")
        );
        Self {
            code: "nonportable-filename".to_string(),
            severity: Severity::Warning,
            path,
            message,
            body: FindingBody::NonportableFilename { issues },
        }
    }
}

#[cfg(test)]
mod link_finding_tests {
    use super::*;
    use crate::domain::{Link, LinkKind, LinkStatus, UnresolvedReason};

    fn link_with(status: LinkStatus, reason: Option<UnresolvedReason>) -> Link {
        Link {
            source_path: "doc.md".into(),
            raw: "[[Target]]".into(),
            kind: LinkKind::Wikilink,
            target: "Target".into(),
            label: None,
            anchor: None,
            block_ref: None,
            source_span: None,
            source_context: None,
            resolved_path: None,
            unresolved_reason: reason,
            candidates: vec![],
            status,
        }
    }

    #[test]
    fn from_link_emits_target_missing_code() {
        let link = link_with(
            LinkStatus::Unresolved,
            Some(UnresolvedReason::TargetMissing),
        );
        let finding = Finding::from_link("doc.md".into(), link);
        assert_eq!(finding.code, "link-target-missing");
        assert!(finding.message.contains("link target not found"));
    }

    #[test]
    fn from_link_emits_anchor_missing_code() {
        let link = link_with(
            LinkStatus::Unresolved,
            Some(UnresolvedReason::AnchorMissing),
        );
        let finding = Finding::from_link("doc.md".into(), link);
        assert_eq!(finding.code, "link-anchor-missing");
    }

    #[test]
    fn from_link_emits_block_missing_code() {
        let link = link_with(
            LinkStatus::Unresolved,
            Some(UnresolvedReason::BlockRefMissing),
        );
        let finding = Finding::from_link("doc.md".into(), link);
        assert_eq!(finding.code, "link-block-missing");
    }

    #[test]
    fn from_link_emits_ambiguous_code() {
        let link = link_with(LinkStatus::Ambiguous, Some(UnresolvedReason::Ambiguous));
        let finding = Finding::from_link("doc.md".into(), link);
        assert_eq!(finding.code, "link-ambiguous");
    }
}
