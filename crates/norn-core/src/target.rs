//! Target resolution and backlink lookup over a built graph.
//!
//! `resolve_target_path` turns a user-supplied target (an exact path or a stem)
//! into a single vault path, erroring on an ambiguous stem; `backlinks`
//! collects the links across the index that resolve to a given path. The
//! mutation appliers and `delete` use these to find what points at a document
//! before they move or remove it.

use crate::domain::{GraphIndex, Link};
use anyhow::{bail, Result};
use camino::Utf8PathBuf;

pub fn backlinks<'a>(index: &'a GraphIndex, target_path: &Utf8PathBuf) -> Vec<&'a Link> {
    index
        .documents
        .iter()
        .flat_map(|document| document.links.iter())
        .filter(|link| link.resolved_path.as_ref() == Some(target_path))
        .collect()
}

/// The outcome of resolving a user-supplied target to a vault path: a unique
/// match, no match, or an ambiguous stem carrying the real candidate paths. The
/// `Ambiguous` arm keeps the candidates structured so a caller can name them in
/// a refusal message rather than reconstructing (or dropping) the set — see
/// [`resolve_target_path`] for the string-shaped wrapper the read verbs use.
pub enum TargetResolution {
    Resolved(Utf8PathBuf),
    NotFound,
    Ambiguous(Vec<Utf8PathBuf>),
}

/// The two refusal families a failed [`TargetResolution`] produces. Every
/// mutating verb (`set`, `edit`, `delete`, `move`) hits one of exactly these
/// two arms when its target/source fails to resolve, each with its own stable
/// `code`.
pub enum TargetRefusalFamily {
    NotFound,
    Ambiguous,
}

impl TargetRefusalFamily {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound => "target-not-found",
            Self::Ambiguous => "target-ambiguous",
        }
    }
}

/// Build the `(code, message)` pair for a target-resolution refusal — the one
/// constructor every mutating verb's resolve-failure branch calls, keyed on
/// which refusal family fired. The `code` is centralized here, declared once
/// instead of re-typed at each call site; `message` is verb-supplied — each
/// caller passes its own wording for what didn't resolve, and this
/// constructor threads it through verbatim.
pub fn target_refusal(family: TargetRefusalFamily, message: String) -> (&'static str, String) {
    (family.code(), message)
}

/// Resolve a target (exact path first, then a case-insensitive stem match) to a
/// structured outcome. The mutation refusal paths (`delete`) branch on this to
/// surface the real ambiguous candidates; [`resolve_target_path`] wraps it for
/// callers that only need the `anyhow` string error.
pub fn resolve_target(index: &GraphIndex, target: &str) -> TargetResolution {
    if let Some(document) = index
        .documents
        .iter()
        .find(|document| document.path == target)
    {
        return TargetResolution::Resolved(document.path.clone());
    }

    let mut matches = index
        .documents
        .iter()
        .filter(|document| document.stem.eq_ignore_ascii_case(target))
        .map(|document| document.path.clone())
        .collect::<Vec<_>>();
    // Lexical path order is the refusal contract; sorting here keeps it
    // independent of the index's own document ordering.
    matches.sort();

    match matches.as_slice() {
        [path] => TargetResolution::Resolved(path.clone()),
        [] => TargetResolution::NotFound,
        _ => TargetResolution::Ambiguous(matches),
    }
}

pub fn resolve_target_path(index: &GraphIndex, target: &str) -> Result<Utf8PathBuf> {
    match resolve_target(index, target) {
        TargetResolution::Resolved(path) => Ok(path),
        TargetResolution::NotFound => bail!("no document matched path or stem: {target}"),
        TargetResolution::Ambiguous(candidates) => bail!(
            "ambiguous document stem: {target}; candidates: {}",
            candidates
                .iter()
                .map(|path| path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Document, GraphIndex, Link, LinkKind, LinkStatus};

    fn doc(path: &str, stem: &str) -> Document {
        Document {
            path: path.into(),
            stem: stem.into(),
            hash: "h".into(),
            frontmatter: None,
            body_text: String::new(),
            headings: vec![],
            block_ids: vec![],
            links: vec![],
            diagnostics: vec![],
            aliases: vec![],
            alias_malformed: vec![],
        }
    }

    fn index(documents: Vec<Document>) -> GraphIndex {
        GraphIndex {
            root: "/vault".into(),
            files: vec![],
            ignored_files: vec![],
            documents,
        }
    }

    fn link_to(source: &str, resolved: Option<&str>) -> Link {
        Link {
            source_path: source.into(),
            raw: "[[x]]".into(),
            kind: LinkKind::Wikilink,
            target: "x".into(),
            label: None,
            anchor: None,
            block_ref: None,
            source_span: None,
            source_context: None,
            resolved_path: resolved.map(Utf8PathBuf::from),
            unresolved_reason: None,
            candidates: vec![],
            status: LinkStatus::Resolved,
        }
    }

    #[test]
    fn resolve_exact_path_wins() {
        let idx = index(vec![doc("notes/a.md", "a"), doc("tasks/a.md", "a")]);
        assert_eq!(
            resolve_target_path(&idx, "notes/a.md").unwrap(),
            Utf8PathBuf::from("notes/a.md")
        );
    }

    #[test]
    fn resolve_unique_stem_case_insensitive() {
        let idx = index(vec![doc("notes/Alpha.md", "Alpha")]);
        assert_eq!(
            resolve_target_path(&idx, "alpha").unwrap(),
            Utf8PathBuf::from("notes/Alpha.md")
        );
    }

    #[test]
    fn resolve_missing_target_errors() {
        let idx = index(vec![doc("notes/a.md", "a")]);
        let err = resolve_target_path(&idx, "missing").unwrap_err();
        assert!(err.to_string().contains("no document matched"), "{err}");
    }

    #[test]
    fn resolve_target_returns_structured_candidates_for_ambiguous_stem() {
        let idx = index(vec![doc("notes/a.md", "a"), doc("tasks/a.md", "a")]);
        match resolve_target(&idx, "a") {
            TargetResolution::Ambiguous(candidates) => assert_eq!(
                candidates,
                vec![
                    Utf8PathBuf::from("notes/a.md"),
                    Utf8PathBuf::from("tasks/a.md"),
                ]
            ),
            _ => panic!("expected Ambiguous with real candidates, got a different arm"),
        }
        assert!(matches!(
            resolve_target(&idx, "missing"),
            TargetResolution::NotFound
        ));
        assert!(matches!(
            resolve_target(&idx, "notes/a.md"),
            TargetResolution::Resolved(_)
        ));
    }

    #[test]
    fn resolve_ambiguous_stem_errors_with_candidates() {
        let idx = index(vec![doc("notes/a.md", "a"), doc("tasks/a.md", "a")]);
        let err = resolve_target_path(&idx, "a").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ambiguous document stem"), "{msg}");
        assert!(
            msg.contains("notes/a.md") && msg.contains("tasks/a.md"),
            "{msg}"
        );
    }

    #[test]
    fn backlinks_collects_links_resolving_to_target() {
        let mut source = doc("notes/src.md", "src");
        source.links = vec![
            link_to("notes/src.md", Some("notes/target.md")),
            link_to("notes/src.md", Some("notes/other.md")),
        ];
        let idx = index(vec![source, doc("notes/target.md", "target")]);
        let found = backlinks(&idx, &Utf8PathBuf::from("notes/target.md"));
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].resolved_path.as_ref().unwrap(),
            &Utf8PathBuf::from("notes/target.md")
        );
    }

    #[test]
    fn backlinks_empty_when_nothing_points_at_target() {
        let idx = index(vec![doc("notes/target.md", "target")]);
        assert!(backlinks(&idx, &Utf8PathBuf::from("notes/target.md")).is_empty());
    }
}
