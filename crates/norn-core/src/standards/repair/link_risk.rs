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

use crate::domain::{Document, Link, LinkKind, VaultFile};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

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
    // Superseded by direct field inspection at call sites; safe to delete in a cleanup pass.
    // No longer needs #[allow(dead_code)]: widened to `pub` (NRN-386 port) and
    // reachable via the crate's public API, so the lint doesn't fire.
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
    pub source_span: Option<crate::domain::SourceSpan>,
    pub rewritten: String,
}

/// Walk the index and classify every link to `old_path` against the `new_path`.
/// Returns the LinkRisk.
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
            // When the link lives inside the file being moved (a self-
            // reference), Pass 3 will need to read the file at its post-move
            // location. Translate the source_path here so the cascade doesn't
            // try to open the old path that Pass 2 has already cleared.
            let source_path = if link.source_path == old_path {
                new_path.to_path_buf()
            } else {
                link.source_path.clone()
            };
            match link.kind {
                LinkKind::Wikilink | LinkKind::Embed => {
                    let is_path_qualified = link.target.contains('/');
                    if is_path_qualified {
                        risk.path_qualified_wikilinks.push(AffectedLink {
                            source_path,
                            raw: link.raw.clone(),
                            kind: link.kind.clone(),
                            source_span: link.source_span,
                            rewritten: rewrite_path_qualified_wikilink(&link.raw, new_path),
                        });
                    } else if risk.stem_changed {
                        risk.stem_links.push(AffectedLink {
                            source_path,
                            raw: link.raw.clone(),
                            kind: link.kind.clone(),
                            source_span: link.source_span,
                            rewritten: rewrite_stem_only_wikilink(&link.raw, new_stem),
                        });
                    }
                }
                LinkKind::Markdown => {
                    // Use the translated source_path (post-move location for
                    // self-references) when computing the relative-path
                    // rewrite. Otherwise a sibling self-link would be turned
                    // into a dangling cross-directory traversal.
                    let rewritten =
                        rewrite_markdown_link(&link.raw, source_path.as_path(), new_path);
                    risk.markdown_links.push(AffectedLink {
                        source_path,
                        raw: link.raw.clone(),
                        kind: link.kind.clone(),
                        source_span: link.source_span,
                        rewritten,
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
    use crate::domain::{Document, Link, LinkKind, LinkSourceArea, LinkSourceContext, LinkStatus};

    fn make_doc(path: &str) -> Document {
        Document {
            path: path.into(),
            stem: camino::Utf8Path::new(path).file_stem().unwrap().to_string(),
            hash: format!("h-{}", path),
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
    fn self_referencing_stem_wikilink_uses_new_path_as_source() {
        // The moved doc references itself in its own body.  The cascade in
        // Pass 3 reads the file at its post-move location, so the AffectedLink
        // must record the new path — otherwise it tries to read a file that
        // Pass 2 just moved out from under it (ENOENT) and the `?` propagation
        // aborts the entire cascade (Bug 1 + Bug 3 from the 2026-05-27 atlas
        // dogfood).
        let old = Utf8PathBuf::from("vault-cli.md");
        let new = Utf8PathBuf::from("norn.md");
        let mut src_doc = make_doc("vault-cli.md");
        src_doc.links.push(wikilink("vault-cli.md", "vault-cli"));

        let documents = vec![src_doc];
        let files = vec![];
        let risk = classify(&old, &new, &documents, &files);

        assert_eq!(risk.stem_links.len(), 1);
        let affected = &risk.stem_links[0];
        assert_eq!(affected.rewritten, "[[norn]]");
        assert_eq!(
            affected.source_path,
            Utf8PathBuf::from("norn.md"),
            "self-reference must read from the new path after move"
        );
    }

    #[test]
    fn self_referencing_path_qualified_wikilink_uses_new_path_as_source() {
        let old = Utf8PathBuf::from("Workspaces/vault-cli/vault-cli.md");
        let new = Utf8PathBuf::from("Workspaces/vault-cli/norn.md");
        let mut src_doc = make_doc("Workspaces/vault-cli/vault-cli.md");
        src_doc.links.push(wikilink(
            "Workspaces/vault-cli/vault-cli.md",
            "Workspaces/vault-cli/vault-cli",
        ));

        let documents = vec![src_doc];
        let files = vec![];
        let risk = classify(&old, &new, &documents, &files);

        assert_eq!(risk.path_qualified_wikilinks.len(), 1);
        let affected = &risk.path_qualified_wikilinks[0];
        assert_eq!(affected.rewritten, "[[Workspaces/vault-cli/norn]]");
        assert_eq!(
            affected.source_path,
            Utf8PathBuf::from("Workspaces/vault-cli/norn.md"),
        );
    }

    #[test]
    fn self_referencing_markdown_link_rewritten_relative_to_new_location() {
        // The moved doc contains a CommonMark link to itself by file name.
        // After the move, the file lives at the new path, so its sibling
        // self-link should still resolve to the (now-new) sibling — i.e. the
        // relative path must be computed from the NEW source directory, not
        // the OLD one. Otherwise the rewrite turns a self-stable link into a
        // dangling cross-directory traversal.
        let old = Utf8PathBuf::from("Inbox/task.md");
        let new = Utf8PathBuf::from("Workspaces/x/tasks/task.md");
        let mut src_doc = make_doc("Inbox/task.md");
        src_doc.links.push(markdown_link_resolved(
            "Inbox/task.md",
            "task.md",
            Some("Inbox/task.md"),
        ));

        let documents = vec![src_doc];
        let files = vec![];
        let risk = classify(&old, &new, &documents, &files);

        assert_eq!(risk.markdown_links.len(), 1);
        let affected = &risk.markdown_links[0];
        assert_eq!(
            affected.source_path,
            Utf8PathBuf::from("Workspaces/x/tasks/task.md")
        );
        assert_eq!(
            affected.rewritten, "[](task.md)",
            "sibling self-link should stay sibling-relative after move"
        );
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
