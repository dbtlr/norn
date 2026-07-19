use super::UnresolvedReason;
#[cfg(test)]
use super::{LinkKind, LinkStatus, Severity};

/// String form of a [`LinkKind`] matching the `#[serde(rename_all = "kebab-case")]` representation.
#[cfg(test)]
fn link_kind_str(kind: &LinkKind) -> &'static str {
    match kind {
        LinkKind::Markdown => "markdown",
        LinkKind::Wikilink => "wikilink",
        LinkKind::Embed => "embed",
    }
}

/// String form of a [`LinkStatus`] matching the `#[serde(rename_all = "kebab-case")]` representation.
#[cfg(test)]
fn link_status_str(status: &LinkStatus) -> &'static str {
    match status {
        LinkStatus::Resolved => "resolved",
        LinkStatus::Unresolved => "unresolved",
        LinkStatus::Ambiguous => "ambiguous",
    }
}

/// String form of a [`Severity`] matching the `#[serde(rename_all = "kebab-case")]` representation.
#[cfg(test)]
fn severity_str(severity: &Severity) -> &'static str {
    match severity {
        Severity::Warning => "warning",
        Severity::Error => "error",
    }
}

/// String form of an [`UnresolvedReason`] matching the `#[serde(rename_all = "kebab-case")]` representation.
pub fn unresolved_reason_str(reason: &UnresolvedReason) -> &'static str {
    match reason {
        UnresolvedReason::TargetMissing => "target-missing",
        UnresolvedReason::AnchorMissing => "anchor-missing",
        UnresolvedReason::BlockRefMissing => "block-ref-missing",
        UnresolvedReason::Ambiguous => "ambiguous",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::{LinkKind, LinkStatus, Severity, UnresolvedReason};

    #[test]
    fn link_kind_str_matches_serde_rename_all() {
        assert_eq!(link_kind_str(&LinkKind::Markdown), "markdown");
        assert_eq!(link_kind_str(&LinkKind::Wikilink), "wikilink");
        assert_eq!(link_kind_str(&LinkKind::Embed), "embed");
    }

    #[test]
    fn link_status_str_matches_serde_rename_all() {
        assert_eq!(link_status_str(&LinkStatus::Resolved), "resolved");
        assert_eq!(link_status_str(&LinkStatus::Unresolved), "unresolved");
        assert_eq!(link_status_str(&LinkStatus::Ambiguous), "ambiguous");
    }

    #[test]
    fn severity_str_matches_serde_rename_all() {
        assert_eq!(severity_str(&Severity::Warning), "warning");
        assert_eq!(severity_str(&Severity::Error), "error");
    }

    #[test]
    fn unresolved_reason_str_matches_serde_rename_all() {
        assert_eq!(
            unresolved_reason_str(&UnresolvedReason::TargetMissing),
            "target-missing"
        );
        assert_eq!(
            unresolved_reason_str(&UnresolvedReason::AnchorMissing),
            "anchor-missing"
        );
        assert_eq!(
            unresolved_reason_str(&UnresolvedReason::BlockRefMissing),
            "block-ref-missing"
        );
        assert_eq!(
            unresolved_reason_str(&UnresolvedReason::Ambiguous),
            "ambiguous"
        );
    }
}
