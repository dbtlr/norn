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

pub fn resolve_target_path(index: &GraphIndex, target: &str) -> Result<Utf8PathBuf> {
    if let Some(document) = index
        .documents
        .iter()
        .find(|document| document.path == target)
    {
        return Ok(document.path.clone());
    }

    let matches = index
        .documents
        .iter()
        .filter(|document| document.stem.eq_ignore_ascii_case(target))
        .map(|document| document.path.clone())
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [path] => Ok(path.clone()),
        [] => bail!("no document matched path or stem: {target}"),
        many => bail!(
            "ambiguous document stem: {target}; candidates: {}",
            many.iter()
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
