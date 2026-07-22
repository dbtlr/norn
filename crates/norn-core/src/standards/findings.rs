//! The validate finding model: one [`Finding`] per rule/link/graph violation.
//!
//! A finding carries a stable `code`, a [`Severity`], the offending document
//! `path`, a human `message`, and named optional fields holding the
//! variant-specific detail the engine's own consumers read (summary, filter,
//! repair). The constructors are the single source of each finding's code +
//! message text; the validate `checks` call them so the wording cannot drift.
//!
//! This is the ENGINE-INTERNAL finding — richer than the output contract because
//! repair needs the compare-and-swap `actual_value` and the skip-prose detail
//! the flat wire contract does not carry (ADR 0022). It never serializes to
//! output: [`Finding::to_wire`] projects it onto the flat [`norn_wire::Finding`]
//! at the output edge, dropping every field the closed wire contract does not
//! name and reducing the internal `Link` / `Diagnostic` models to plain strings.

use crate::domain::{Diagnostic, Link, LinkStatus, Severity, UnresolvedReason};
use camino::Utf8PathBuf;
use serde_json::Value;

/// A single validation finding (engine-internal; see the module docs). Flat: the
/// earlier untagged `FindingBody` split was retired (ADR 0022). Absent fields are
/// `None` / empty; each constructor populates only the slots its code uses.
#[derive(Debug, Clone)]
pub struct Finding {
    pub code: String,
    pub severity: Severity,
    pub path: Utf8PathBuf,
    pub message: String,
    /// Named validate rule that produced the finding (rule-scoped frontmatter
    /// findings only).
    pub rule: Option<String>,
    /// Frontmatter field the finding concerns.
    pub field: Option<String>,
    /// Link target (link findings).
    pub target: Option<String>,
    /// Link unresolved reason (link findings) — drives the `--reason` filter.
    pub reason: Option<UnresolvedReason>,
    /// Resolution candidates (link findings) as vault paths.
    pub candidates: Vec<Utf8PathBuf>,
    /// Offending frontmatter value (value/type/length/forbidden findings) —
    /// repair reads it as the compare-and-swap `expected_old_value` and for
    /// `actual_value`-scoped rule matching.
    pub actual_value: Option<Value>,
    /// Declared expected type (invalid-type findings) — the summary breakdown.
    pub expected_type: Option<String>,
    /// Allowed target types (reference-type findings) — repair skip prose.
    pub allowed_types: Vec<String>,
    /// Path-portability issues (nonportable-filename findings) — repair skip prose.
    pub issues: Vec<String>,
    /// Alias value (alias findings) — repair skip prose.
    pub alias_value: Option<String>,
    /// Doc whose stem shadows the alias (alias-shadowed findings) — repair prose.
    pub shadowing_doc_path: Option<Utf8PathBuf>,
}

impl Finding {
    fn base(code: &str, severity: Severity, path: Utf8PathBuf, message: String) -> Self {
        Self {
            code: code.to_string(),
            severity,
            path,
            message,
            rule: None,
            field: None,
            target: None,
            reason: None,
            candidates: Vec::new(),
            actual_value: None,
            expected_type: None,
            allowed_types: Vec::new(),
            issues: Vec::new(),
            alias_value: None,
            shadowing_doc_path: None,
        }
    }

    /// Project onto the flat wire contract (ADR 0022): plain-string paths, the
    /// closed field set, no internal models. Fields the contract does not name
    /// (`actual_value`, `expected_type`, `allowed_types`, `issues`, `reason`, …)
    /// are dropped — their decision value already folded into `message` at
    /// construction where it mattered.
    pub fn to_wire(&self) -> norn_wire::Finding {
        norn_wire::Finding {
            path: self.path.as_str().to_string(),
            code: self.code.clone(),
            severity: to_wire_severity(self.severity),
            message: self.message.clone(),
            rule: self.rule.clone(),
            field: self.field.clone(),
            target: self.target.clone(),
            candidates: self
                .candidates
                .iter()
                .map(|p| p.as_str().to_string())
                .collect(),
            // Validate findings carry no actionable hints today; the slot exists
            // for the closed contract's shape.
            next_actions: Vec::new(),
        }
    }

    pub fn from_graph_diagnostic(path: Utf8PathBuf, diagnostic: Diagnostic) -> Self {
        // The diagnostic's `detail` (a parser cause string) has no slot in the
        // flat contract and is not read by any engine consumer — it drops. The
        // `code`, `severity`, and `message` carry forward.
        Self::base(
            &diagnostic.code,
            diagnostic.severity,
            path,
            diagnostic.message,
        )
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

        let mut finding = Self::base(code, Severity::Warning, path, message);
        finding.target = Some(link.target);
        finding.reason = link.unresolved_reason;
        finding.candidates = link.candidates;
        finding
    }

    pub fn frontmatter_required_missing(
        path: Utf8PathBuf,
        rule: Option<String>,
        field: String,
    ) -> Self {
        let message = format!("required frontmatter field is missing: {field}");
        let mut finding = Self::base(
            "frontmatter-required-field-missing",
            Severity::Warning,
            path,
            message,
        );
        finding.rule = rule;
        finding.field = Some(field);
        finding
    }

    pub fn frontmatter_disallowed_value(
        path: Utf8PathBuf,
        rule: Option<String>,
        field: String,
        actual_value: Value,
        _allowed_values: Vec<Value>,
    ) -> Self {
        let message = format!("frontmatter field has a disallowed value: {field}");
        let mut finding = Self::base("value-not-allowed", Severity::Warning, path, message);
        finding.rule = rule;
        finding.field = Some(field);
        finding.actual_value = Some(actual_value);
        finding
    }

    /// A frontmatter wikilink resolves to a document whose `type` is outside
    /// the field's `field_references.target_type` set.
    #[allow(clippy::too_many_arguments)]
    pub fn frontmatter_reference_type(
        path: Utf8PathBuf,
        rule: Option<String>,
        field: String,
        _reference: String,
        target: Utf8PathBuf,
        actual_type: String,
        allowed_types: Vec<String>,
    ) -> Self {
        let message = format!(
            "frontmatter field references a document of a disallowed type: {field} → {target} (type: {actual_type})"
        );
        let mut finding = Self::base(
            "frontmatter-reference-type",
            Severity::Warning,
            path,
            message,
        );
        finding.rule = rule;
        finding.field = Some(field);
        finding.allowed_types = allowed_types;
        finding
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
        let mut finding = Self::base("field-type-invalid", Severity::Warning, path, message);
        finding.rule = rule;
        finding.field = Some(field);
        finding.actual_value = Some(actual_value);
        finding.expected_type = Some(expected_type);
        finding
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
        let mut finding = Self::base(
            "frontmatter-exceeds-max-length",
            Severity::Warning,
            path,
            message,
        );
        finding.rule = rule;
        finding.field = Some(field);
        finding.actual_value = Some(actual_value);
        finding
    }

    pub fn frontmatter_forbidden_field(
        path: Utf8PathBuf,
        rule: Option<String>,
        field: String,
        actual_value: Value,
    ) -> Self {
        let message = format!("frontmatter field is forbidden: {field}");
        let mut finding = Self::base(
            "frontmatter-forbidden-field",
            Severity::Warning,
            path,
            message,
        );
        finding.rule = rule;
        finding.field = Some(field);
        finding.actual_value = Some(actual_value);
        finding
    }

    pub fn document_misrouted(
        path: Utf8PathBuf,
        rule: Option<String>,
        _allowed_paths: Vec<String>,
    ) -> Self {
        let mut finding = Self::base(
            "document-misrouted",
            Severity::Warning,
            path,
            "document path is outside allowed rule locations".to_string(),
        );
        finding.rule = rule;
        finding
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
        let mut finding = Self::base(
            "frontmatter-alias-malformed",
            Severity::Warning,
            path,
            message,
        );
        finding.field = Some(field);
        finding
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
        let mut finding = Self::base(
            "frontmatter-alias-shadowed-by-stem",
            Severity::Warning,
            path,
            message,
        );
        finding.alias_value = Some(alias_value);
        finding.shadowing_doc_path = Some(shadowing_doc_path);
        finding
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
        let mut finding = Self::base(
            "frontmatter-alias-duplicate-across-docs",
            Severity::Warning,
            path,
            message,
        );
        finding.alias_value = Some(alias_value);
        finding
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
        let mut finding = Self::base("nonportable-filename", Severity::Warning, path, message);
        finding.issues = issues;
        finding
    }
}

fn to_wire_severity(severity: Severity) -> norn_wire::Severity {
    match severity {
        Severity::Warning => norn_wire::Severity::Warning,
        Severity::Error => norn_wire::Severity::Error,
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
        assert_eq!(finding.target.as_deref(), Some("Target"));
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

    #[test]
    fn to_wire_drops_internal_fields_and_stringifies_paths() {
        let mut link = link_with(LinkStatus::Ambiguous, Some(UnresolvedReason::Ambiguous));
        link.candidates = vec!["a/Target.md".into(), "b/Target.md".into()];
        let finding = Finding::from_link("doc.md".into(), link);
        let wire = finding.to_wire();
        assert_eq!(wire.code, "link-ambiguous");
        assert_eq!(wire.path, "doc.md");
        assert_eq!(wire.target.as_deref(), Some("Target"));
        assert_eq!(wire.candidates, vec!["a/Target.md", "b/Target.md"]);
        assert!(wire.rule.is_none());
        assert!(wire.field.is_none());

        // The internal-only fields never reach the wire object.
        let v = serde_json::to_value(&wire).unwrap();
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("reason"));
        assert!(!obj.contains_key("actual_value"));
        assert!(!obj.contains_key("status"));
        assert!(!obj.contains_key("link"));
    }

    #[test]
    fn to_wire_carries_rule_and_field_for_frontmatter_findings() {
        let finding = Finding::frontmatter_required_missing(
            "task.md".into(),
            Some("typed-note".into()),
            "title".into(),
        );
        let wire = finding.to_wire();
        assert_eq!(wire.code, "frontmatter-required-field-missing");
        assert_eq!(wire.rule.as_deref(), Some("typed-note"));
        assert_eq!(wire.field.as_deref(), Some("title"));
        // actual_value is engine-internal and does not cross to the wire.
        let v = serde_json::to_value(&wire).unwrap();
        assert!(!v.as_object().unwrap().contains_key("actual_value"));
    }
}
