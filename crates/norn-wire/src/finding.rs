//! The validation finding — the one flat, closed contract every surface speaks.
//!
//! ADR 0022: findings cross the CLI JSON path, the daemon wire, and the MCP
//! tool as ONE flat struct with a closed field set. There is no untagged enum,
//! no variant-specific field set, and no internal model (link-resolution state,
//! spans, parse context) embedded in output. `candidates` are plain vault-path
//! strings and `next_actions` are plain strings, so the type is nameable and
//! `JsonSchema`-able — the MCP validate tool exposes it as a schema rather than
//! an opaque value. The engine's richer internal finding projects onto this at
//! the output edge; this type never carries planner internals.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Finding severity — the kebab-case `warning` / `error` vocabulary shared with
/// the text-layer diagnostics. Mirrored here (not imported) because the wire
/// crate depends on no other norn crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
    Warning,
    Error,
}

/// One validation finding, flat and closed (ADR 0022). Absent optional fields
/// are omitted from serialization, so a finding carries only the slots its code
/// populates. `path`, `target`, and `candidates` are plain strings — the
/// internal `Link` / `Diagnostic` models never serialize into a finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Finding {
    /// Vault-relative path of the offending document.
    pub path: String,
    /// Stable kebab-case code a consumer branches on (e.g. `link-target-missing`,
    /// `frontmatter-required-field-missing`).
    pub code: String,
    /// `warning` or `error`.
    pub severity: Severity,
    /// Human-readable one-line description.
    pub message: String,
    /// Named validate rule that produced the finding, when the code is
    /// rule-scoped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    /// Frontmatter field the finding concerns, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// Link target the finding concerns, for link findings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Resolution candidates, as plain vault paths — for ambiguous / closest-match
    /// link findings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<String>,
    /// Actionable next steps a consumer can surface, plain strings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_actions: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_optionals_are_omitted() {
        let f = Finding {
            path: "notes/a.md".into(),
            code: "link-target-missing".into(),
            severity: Severity::Warning,
            message: "link target not found: Foo".into(),
            rule: None,
            field: None,
            target: Some("Foo".into()),
            candidates: vec![],
            next_actions: vec![],
        };
        let v = serde_json::to_value(&f).unwrap();
        let obj = v.as_object().unwrap();
        assert!(obj.contains_key("target"));
        assert!(!obj.contains_key("rule"), "absent rule is omitted");
        assert!(!obj.contains_key("field"), "absent field is omitted");
        assert!(!obj.contains_key("candidates"), "empty candidates omitted");
        assert!(
            !obj.contains_key("next_actions"),
            "empty next_actions omitted"
        );
    }

    #[test]
    fn severity_is_kebab_case() {
        assert_eq!(
            serde_json::to_value(Severity::Warning).unwrap(),
            serde_json::json!("warning")
        );
        assert_eq!(
            serde_json::to_value(Severity::Error).unwrap(),
            serde_json::json!("error")
        );
    }

    #[test]
    fn round_trips_through_serde() {
        let f = Finding {
            path: "notes/a.md".into(),
            code: "value-not-allowed".into(),
            severity: Severity::Error,
            message: "frontmatter field has a disallowed value: status".into(),
            rule: Some("task-status".into()),
            field: Some("status".into()),
            target: None,
            candidates: vec![],
            next_actions: vec!["set status to a permitted value".into()],
        };
        let s = serde_json::to_string(&f).unwrap();
        let back: Finding = serde_json::from_str(&s).unwrap();
        assert_eq!(f, back);
    }
}
