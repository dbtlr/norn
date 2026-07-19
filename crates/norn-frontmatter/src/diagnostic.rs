//! Text-layer diagnostics.
//!
//! Frontmatter extraction is forgiving — a malformed block yields no value plus
//! a warning rather than an error return — so the parse surface accumulates
//! [`Diagnostic`]s the caller can surface or ignore. This is the text layer's
//! own lean diagnostic type; it never depends on a vault, schema, or CLI.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
    Warning,
    Error,
}

/// A coded, human-readable note produced while parsing text. `code` is a stable
/// kebab identifier a caller can branch on; `message` is prose; `detail` carries
/// an optional underlying cause (e.g. a YAML parser's error string).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Diagnostic {
    pub fn warning(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            code: code.into(),
            message: message.into(),
            detail: None,
        }
    }

    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            code: code.into(),
            message: message.into(),
            detail: None,
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_constructor_sets_error_severity() {
        let diagnostic = Diagnostic::error("read-failed", "failed to read document");
        assert_eq!(diagnostic.severity, Severity::Error);
        assert_eq!(diagnostic.code, "read-failed");
        assert_eq!(diagnostic.message, "failed to read document");
        assert_eq!(diagnostic.detail, None);
    }

    #[test]
    fn error_carries_optional_detail() {
        let diagnostic =
            Diagnostic::error("read-failed", "failed to read document").with_detail("boom");
        assert_eq!(diagnostic.detail.as_deref(), Some("boom"));
    }

    #[test]
    fn warning_constructor_sets_warning_severity() {
        let diagnostic = Diagnostic::warning("frontmatter-parse-failed", "malformed");
        assert_eq!(diagnostic.severity, Severity::Warning);
    }
}
