//! Mutation-verb execute seams (set/new/edit/move/delete/rewrite-wikilink). Each
//! builds — and, when confirmed, applies — a MigrationPlan against the warm
//! cache, returning a report. The frontmatter/body verbs (`set`/`new`/`edit`)
//! answer with a compact wire twin; the cascade verbs
//! (`move`/`delete`/`rewrite_wikilink`) answer with the shared
//! [`norn_wire::ApplyReport`], which the owner
//! serializes onto the wire as an opaque JSON value.
pub mod apply;
pub(crate) mod coerce;
pub mod delete;
pub mod edit;
pub mod move_doc;
pub mod new;
pub mod rewrite_wikilink;
pub mod set;

use camino::Utf8PathBuf;

/// What the owner drives: the verb's wire Report plus the paths a CONFIRMED
/// write touched (empty for a forecast or a refusal) — the owner commits the
/// cache increment for exactly these.
pub struct MutationExecution<R> {
    pub report: R,
    pub touched_paths: Vec<Utf8PathBuf>,
}

/// Warn-class wikilink resolution: does `value`'s stem resolve to a unique doc?
/// Empty → unresolved; >1 → ambiguous; exactly one → no warning. Shared by both
/// mutation seams.
pub(crate) fn wikilink_warnings(
    index: &crate::domain::GraphIndex,
    field: &str,
    value: &str,
) -> Vec<norn_wire::MutationWarning> {
    // NRN-412(c): the resolution stem comes from the authoritative parser, not a
    // hand-rolled `[[..]]` strip. The parsed-link branch applies ONLY when the
    // value is EXACTLY one wikilink spanning the whole input (one token whose raw
    // == value): a multi-token value like `[[a]] [[b]]` must not silently resolve
    // on just `a`, so it falls through to the raw-value branch (which fails to
    // resolve and warns honestly). A bare value (Obsidian permits an unbracketed
    // target) is likewise treated as the target text directly.
    let tokens = norn_frontmatter::wikilink::parse_wikilinks_in_text(value);
    let single = match tokens.as_slice() {
        [token] if token.raw == value => Some(token),
        _ => None,
    };
    let (display, canonical) = match single {
        Some(token) => (token.raw.clone(), token.target.to_lowercase()),
        None => {
            let bare = value
                .split('#')
                .next()
                .unwrap_or(value)
                .split('|')
                .next()
                .unwrap_or(value);
            (format!("[[{value}]]"), bare.to_lowercase())
        }
    };
    let matches = index
        .documents
        .iter()
        .filter(|d| d.stem.to_lowercase() == canonical)
        .count();
    match matches {
        0 => vec![norn_wire::MutationWarning {
            code: "wikilink-unresolved".into(),
            field: Some(field.to_string()),
            message: format!("unresolved wikilink in {field}: {display}"),
        }],
        1 => Vec::new(),
        _ => vec![norn_wire::MutationWarning {
            code: "wikilink-ambiguous".into(),
            field: Some(field.to_string()),
            message: format!("ambiguous wikilink in {field}: {display}"),
        }],
    }
}

/// The owner index options derived from a vault config's ignore + alias-field —
/// the second-scan policy `apply_migration_plan` uses for the owner-set barrier.
/// A verb with no logical preconditions never pays for that scan, but the value
/// is cheap to build and keeps the two verbs identical.
pub(crate) fn owner_index_options(
    config: Option<&crate::standards::VaultConfig>,
) -> crate::graph::IndexOptions {
    match config {
        Some(cfg) => crate::graph::IndexOptions {
            ignore: cfg.files.ignore.clone(),
            alias_field: cfg.links.alias_field.clone(),
        },
        None => crate::graph::IndexOptions::default(),
    }
}

#[cfg(test)]
mod wikilink_warning_tests {
    use tempfile::TempDir;

    fn index_with_target() -> (TempDir, crate::domain::GraphIndex) {
        let tmp = tempfile::Builder::new()
            .prefix("mutate-wikilink-warn-")
            .tempdir()
            .unwrap();
        let root = tmp.path();
        std::fs::write(root.join("target.md"), "---\ntype: note\n---\n# Target\n").unwrap();
        let index = crate::graph::build_index(camino::Utf8Path::from_path(root).unwrap()).unwrap();
        (tmp, index)
    }

    /// NRN-412(c): the warn-class resolver decomposes the value through the
    /// authoritative parser, so a bracketed value with an anchor + alias resolves
    /// on the bare target stem — a `[[target#Heading|Alias]]` value points at the
    /// one `target.md`, no warning. A hand-rolled strip that kept `#Heading|Alias`
    /// in the stem would report it unresolved.
    #[test]
    fn bracketed_value_with_anchor_and_alias_resolves_via_parser() {
        let (_tmp, index) = index_with_target();
        assert!(
            super::wikilink_warnings(&index, "related", "[[target#Heading|Alias]]").is_empty(),
            "anchor + alias must be split off the resolution stem"
        );
    }

    /// The unresolved message shows the parsed link's raw verbatim (brackets,
    /// anchor, and alias intact).
    #[test]
    fn unresolved_message_shows_the_parsed_raw() {
        let (_tmp, index) = index_with_target();
        let warnings = super::wikilink_warnings(&index, "related", "[[missing#Heading|Alias]]");
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code, "wikilink-unresolved");
        assert_eq!(
            warnings[0].message,
            "unresolved wikilink in related: [[missing#Heading|Alias]]"
        );
    }

    /// A bare (unbracketed) value is still treated as the target text directly.
    #[test]
    fn bare_value_resolves_directly() {
        let (_tmp, index) = index_with_target();
        assert!(super::wikilink_warnings(&index, "related", "target").is_empty());
    }

    /// A multi-token value must NOT silently resolve on its first link: the
    /// parsed-link branch only fires for a single full-span token, so `[[target]]
    /// [[other]]` falls through to the raw-value branch and warns unresolved
    /// (rather than the pre-guard bug where it resolved on `target` alone).
    #[test]
    fn multi_token_value_does_not_resolve_on_first_link() {
        let (_tmp, index) = index_with_target();
        let warnings = super::wikilink_warnings(&index, "related", "[[target]] [[other]]");
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code, "wikilink-unresolved");
        assert_eq!(
            warnings[0].message,
            "unresolved wikilink in related: [[[[target]] [[other]]]]"
        );
    }
}
