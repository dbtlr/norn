use camino::Utf8PathBuf;
use serde::Serialize;
use serde_json::Value;
use vault_core::{Diagnostic, Link, Severity};

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
    ForbiddenField {
        rule: Option<String>,
        field: String,
        actual_value: Value,
    },
    DocumentMisrouted {
        rule: Option<String>,
        allowed_paths: Vec<String>,
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
}

impl Finding {
    pub fn from_graph_diagnostic(path: Utf8PathBuf, diagnostic: Diagnostic) -> Self {
        Self {
            code: diagnostic.code.clone(),
            severity: diagnostic.severity.clone(),
            message: diagnostic.message.clone(),
            path,
            body: FindingBody::GraphDiagnostic { diagnostic },
        }
    }

    pub fn link_unresolved(path: Utf8PathBuf, link: Link) -> Self {
        let message = format!("unresolved link target: {}", link.target);
        Self {
            code: "link-unresolved".to_string(),
            severity: Severity::Warning,
            path,
            message,
            body: FindingBody::LinkIssue { link },
        }
    }

    pub fn link_ambiguous(path: Utf8PathBuf, link: Link) -> Self {
        let message = format!("ambiguous link target: {}", link.target);
        Self {
            code: "link-ambiguous".to_string(),
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
            code: "frontmatter-disallowed-value".to_string(),
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
            code: "frontmatter-invalid-type".to_string(),
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
}
