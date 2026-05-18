//! Link risk classification for move_document changes.
//!
//! Given an old → new path pair, walks the index's link graph and partitions
//! every link that targets the source into three categories:
//!
//! - **stem_links** — stem-only wikilinks; affected only when the stem changes.
//! - **path_qualified_wikilinks** — wikilinks with `/` in their target; affected
//!   whenever the path portion that contains the source changes.
//! - **markdown_links** — Markdown links referencing the source path; affected
//!   whenever any path component changes.
//!
//! For each affected link, the rewritten replacement text is precomputed so
//! apply can perform a literal string-replace at write time.

use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use vault_core::{Document, Link, LinkKind, VaultFile};

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct LinkRisk {
    pub stem_changed: bool,
    pub directory_changed: bool,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub stem_links: Vec<AffectedLink>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub path_qualified_wikilinks: Vec<AffectedLink>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub markdown_links: Vec<AffectedLink>,
}

impl LinkRisk {
    pub fn has_affected(&self) -> bool {
        !self.stem_links.is_empty()
            || !self.path_qualified_wikilinks.is_empty()
            || !self.markdown_links.is_empty()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AffectedLink {
    pub source_path: Utf8PathBuf,
    pub raw: String,
    pub kind: LinkKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_span: Option<vault_core::SourceSpan>,
    pub rewritten: String,
}

/// Walk the index and classify every link to `old_path` against the `new_path`.
/// Returns the LinkRisk.
///
/// Rewrite blockers (path traversal, glob chars, links outside the vault inventory)
/// are NOT detected here — they're surfaced at the planner level (Task 8).
pub fn classify(
    old_path: &Utf8Path,
    new_path: &Utf8Path,
    documents: &[Document],
    _files: &[VaultFile],
) -> LinkRisk {
    let old_stem = old_path.file_stem().unwrap_or("");
    let new_stem = new_path.file_stem().unwrap_or("");
    let old_dir = old_path.parent().unwrap_or(Utf8Path::new(""));
    let new_dir = new_path.parent().unwrap_or(Utf8Path::new(""));

    let mut risk = LinkRisk {
        stem_changed: old_stem != new_stem,
        directory_changed: old_dir != new_dir,
        ..Default::default()
    };

    for doc in documents {
        for link in &doc.links {
            if !link_targets_path(link, old_path) {
                continue;
            }
            match link.kind {
                LinkKind::Wikilink | LinkKind::Embed => {
                    let is_path_qualified = link.target.contains('/');
                    if is_path_qualified {
                        risk.path_qualified_wikilinks.push(AffectedLink {
                            source_path: link.source_path.clone(),
                            raw: link.raw.clone(),
                            kind: link.kind.clone(),
                            source_span: link.source_span.clone(),
                            rewritten: rewrite_path_qualified_wikilink(&link.raw, new_path),
                        });
                    } else if risk.stem_changed {
                        risk.stem_links.push(AffectedLink {
                            source_path: link.source_path.clone(),
                            raw: link.raw.clone(),
                            kind: link.kind.clone(),
                            source_span: link.source_span.clone(),
                            rewritten: rewrite_stem_only_wikilink(&link.raw, new_stem),
                        });
                    }
                }
                LinkKind::Markdown => {
                    risk.markdown_links.push(AffectedLink {
                        source_path: link.source_path.clone(),
                        raw: link.raw.clone(),
                        kind: link.kind.clone(),
                        source_span: link.source_span.clone(),
                        rewritten: rewrite_markdown_link(
                            &link.raw,
                            &link.source_path,
                            new_path,
                        ),
                    });
                }
            }
        }
    }

    risk
}

fn link_targets_path(link: &Link, target: &Utf8Path) -> bool {
    if let Some(resolved) = &link.resolved_path {
        return resolved == target;
    }
    // Fallback for tests / unresolved links: compare the raw target literally.
    let target_str = target.as_str();
    let target_stem = target.file_stem().unwrap_or("");
    link.target == target_str
        || link.target == target_stem
        || link.target.trim_end_matches(".md") == target_str.trim_end_matches(".md")
}

fn rewrite_stem_only_wikilink(raw: &str, new_stem: &str) -> String {
    let inner = raw
        .strip_prefix("[[")
        .and_then(|s| s.strip_suffix("]]"))
        .unwrap_or("");
    let (_target_part, rest) = match inner.find(['|', '#']) {
        Some(idx) => inner.split_at(idx),
        None => (inner, ""),
    };
    format!("[[{new_stem}{rest}]]")
}

fn rewrite_path_qualified_wikilink(raw: &str, new: &Utf8Path) -> String {
    let inner = raw
        .strip_prefix("[[")
        .and_then(|s| s.strip_suffix("]]"))
        .unwrap_or("");
    let (_target_part, rest) = match inner.find(['|', '#']) {
        Some(idx) => inner.split_at(idx),
        None => (inner, ""),
    };
    let new_target = new.as_str().trim_end_matches(".md").to_string();
    format!("[[{new_target}{rest}]]")
}

fn rewrite_markdown_link(raw: &str, source_file: &Utf8Path, new: &Utf8Path) -> String {
    let source_dir = source_file.parent().unwrap_or(Utf8Path::new(""));
    let new_relative = compute_relative_path(source_dir, new);

    // Case 1: full bracketed form (`[text](url)`). Some callers (and the
    // legacy unit tests) pass `raw` as the entire link. Handle that shape
    // first so we don't accidentally treat it as a bare URL.
    if let Some(bracket_close) = raw.find("](") {
        let text_part = &raw[..bracket_close + 2];
        let path_and_anchor = &raw[bracket_close + 2..];
        if let Some(paren_close) = path_and_anchor.rfind(')') {
            let inside = &path_and_anchor[..paren_close];
            let (_, anchor) = match inside.find('#') {
                Some(i) => inside.split_at(i),
                None => (inside, ""),
            };
            return format!("{text_part}{new_relative}{anchor})");
        }
    }

    // Case 2: bare URL form. This is what `vault-links` actually stores for
    // CommonMark links (`Link.raw = dest_url`). Preserve any `#anchor`
    // segment so heading targets aren't dropped on move.
    let (_url_part, anchor) = match raw.find('#') {
        Some(i) => raw.split_at(i),
        None => (raw, ""),
    };
    format!("{new_relative}{anchor}")
}

fn compute_relative_path(from: &Utf8Path, to: &Utf8Path) -> String {
    let from_comps: Vec<_> = from.components().collect();
    let to_comps: Vec<_> = to.components().collect();
    let common = from_comps
        .iter()
        .zip(to_comps.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let ups = from_comps.len() - common;
    let mut result = String::new();
    for _ in 0..ups {
        result.push_str("../");
    }
    for comp in &to_comps[common..] {
        if !result.is_empty() && !result.ends_with('/') {
            result.push('/');
        }
        result.push_str(comp.as_str());
    }
    if result.is_empty() {
        result.push_str(to.file_name().unwrap_or(""));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use vault_core::{Document, Link, LinkKind, LinkSourceArea, LinkSourceContext, LinkStatus};

    fn make_doc(path: &str) -> Document {
        Document {
            path: path.into(),
            stem: camino::Utf8Path::new(path)
                .file_stem()
                .unwrap()
                .to_string(),
            hash: format!("h-{}", path),
            frontmatter: None,
            headings: vec![],
            block_ids: vec![],
            links: vec![],
            diagnostics: vec![],
        }
    }

    fn wikilink(source: &str, target: &str) -> Link {
        Link {
            source_path: source.into(),
            raw: format!("[[{target}]]"),
            kind: LinkKind::Wikilink,
            target: target.into(),
            label: None,
            anchor: None,
            block_ref: None,
            source_span: None,
            source_context: Some(LinkSourceContext {
                area: LinkSourceArea::Body,
                property: None,
            }),
            resolved_path: None,
            unresolved_reason: None,
            candidates: vec![],
            status: LinkStatus::Resolved,
        }
    }

    fn markdown_link(source: &str, target: &str) -> Link {
        markdown_link_resolved(source, target, None)
    }

    fn markdown_link_resolved(source: &str, target: &str, resolved: Option<&str>) -> Link {
        Link {
            source_path: source.into(),
            raw: format!("[]({target})"),
            kind: LinkKind::Markdown,
            target: target.into(),
            label: None,
            anchor: None,
            block_ref: None,
            source_span: None,
            source_context: Some(LinkSourceContext {
                area: LinkSourceArea::Body,
                property: None,
            }),
            resolved_path: resolved.map(Into::into),
            unresolved_reason: None,
            candidates: vec![],
            status: LinkStatus::Resolved,
        }
    }

    #[test]
    fn pure_directory_move_stem_only_wikilinks_unaffected() {
        let old = Utf8PathBuf::from("Inbox/task.md");
        let new = Utf8PathBuf::from("Workspaces/x/tasks/task.md");
        let mut idx_doc = make_doc("Inbox/index.md");
        idx_doc.links.push(wikilink("Inbox/index.md", "task"));

        let documents = vec![idx_doc];
        let files = vec![];
        let risk = classify(&old, &new, &documents, &files);

        assert!(risk.directory_changed);
        assert!(!risk.stem_changed);
        assert!(risk.stem_links.is_empty());
        assert!(risk.path_qualified_wikilinks.is_empty());
        assert!(risk.markdown_links.is_empty());
    }

    #[test]
    fn pure_directory_move_path_qualified_wikilinks_affected() {
        let old = Utf8PathBuf::from("Inbox/task.md");
        let new = Utf8PathBuf::from("Workspaces/x/tasks/task.md");
        let mut idx_doc = make_doc("Inbox/index.md");
        idx_doc.links.push(wikilink("Inbox/index.md", "Inbox/task"));

        let documents = vec![idx_doc];
        let files = vec![];
        let risk = classify(&old, &new, &documents, &files);

        assert_eq!(risk.path_qualified_wikilinks.len(), 1);
        let affected = &risk.path_qualified_wikilinks[0];
        assert_eq!(affected.raw, "[[Inbox/task]]");
        assert_eq!(affected.rewritten, "[[Workspaces/x/tasks/task]]");
    }

    #[test]
    fn pure_rename_all_wikilinks_affected() {
        let old = Utf8PathBuf::from("Notes/task.md");
        let new = Utf8PathBuf::from("Notes/next-task.md");
        let mut idx_doc = make_doc("Notes/index.md");
        idx_doc.links.push(wikilink("Notes/index.md", "task"));
        idx_doc.links.push(wikilink("Notes/index.md", "Notes/task"));

        let documents = vec![idx_doc];
        let files = vec![];
        let risk = classify(&old, &new, &documents, &files);

        assert!(risk.stem_changed);
        assert!(!risk.directory_changed);
        assert_eq!(risk.stem_links.len(), 1);
        assert_eq!(risk.stem_links[0].rewritten, "[[next-task]]");
        assert_eq!(risk.path_qualified_wikilinks.len(), 1);
        assert_eq!(
            risk.path_qualified_wikilinks[0].rewritten,
            "[[Notes/next-task]]"
        );
    }

    #[test]
    fn markdown_links_affected_on_any_path_change() {
        let old = Utf8PathBuf::from("Inbox/task.md");
        let new = Utf8PathBuf::from("Workspaces/x/tasks/task.md");
        let mut idx_doc = make_doc("Inbox/index.md");
        idx_doc.links.push(markdown_link_resolved(
            "Inbox/index.md",
            "task.md",
            Some("Inbox/task.md"),
        ));
        idx_doc.links.push(markdown_link_resolved(
            "Inbox/index.md",
            "../Inbox/task.md",
            Some("Inbox/task.md"),
        ));

        let documents = vec![idx_doc];
        let files = vec![];
        let risk = classify(&old, &new, &documents, &files);

        assert_eq!(risk.markdown_links.len(), 2);
    }

    #[test]
    fn markdown_link_with_bare_url_raw_rewrites_correctly() {
        // Simulates vault-links' actual production behavior: raw == bare URL,
        // not the full [label](url) form. This is what apply_link_rewrites
        // will substring-match against the source file's content.
        let raw = "task.md";
        let rewritten = rewrite_markdown_link(
            raw,
            Utf8Path::new("Inbox/index.md"),
            Utf8Path::new("Workspaces/demo/tasks/task.md"),
        );
        assert_eq!(rewritten, "../Workspaces/demo/tasks/task.md");
    }

    #[test]
    fn markdown_link_bare_url_preserves_anchor() {
        let raw = "task.md#heading";
        let rewritten = rewrite_markdown_link(
            raw,
            Utf8Path::new("Inbox/index.md"),
            Utf8Path::new("Workspaces/demo/tasks/task.md"),
        );
        assert_eq!(rewritten, "../Workspaces/demo/tasks/task.md#heading");
    }

    #[test]
    fn classify_does_not_panic_on_unusual_inputs() {
        let old = Utf8PathBuf::from("Inbox/task.md");
        let new = Utf8PathBuf::from("Workspaces/x/tasks/task.md");
        let mut idx_doc = make_doc("Inbox/index.md");
        idx_doc
            .links
            .push(markdown_link("Inbox/index.md", "../../outside.md"));

        let documents = vec![idx_doc];
        let files = vec![];
        let risk = classify(&old, &new, &documents, &files);
        let _ = risk;
    }
}
