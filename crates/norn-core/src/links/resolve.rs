//! Link resolution — matching a parsed [`Link`]'s target to a document in the
//! vault, deciding resolved / unresolved / ambiguous status.
//!
//! This is the semantic half of the link model (the lexical half — token and
//! anchor syntax — is `norn_frontmatter`). Resolution is external contract: which
//! path a target resolves to, when it is ambiguous, and which anchors/blocks
//! validate are observable behavior held to donor parity. Known contract-shaped
//! quirks tracked on the correctness slate — block-ids inside code spans
//! (NRN-350), alias / ambiguity edges (NRN-122 / NRN-124) — are preserved here
//! deliberately, not silently changed.

use std::collections::HashMap;
use std::path::Component;

use crate::domain::{Document, Link, LinkKind, LinkStatus, UnresolvedReason, VaultFile};
use camino::{Utf8Path, Utf8PathBuf};
use norn_frontmatter::heading::slugify;

/// Resolve every link on every document in place against the vault's file and
/// document tables. Populates each link's `status`, `resolved_path`,
/// `unresolved_reason`, and `candidates`.
pub fn resolve_links(files: &[VaultFile], documents: &mut [Document]) {
    let mut by_path: HashMap<String, Utf8PathBuf> = HashMap::new();
    let mut by_path_lower: HashMap<String, Utf8PathBuf> = HashMap::new();
    let mut by_stem: HashMap<String, Vec<Utf8PathBuf>> = HashMap::new();
    let mut by_alias: HashMap<String, Vec<Utf8PathBuf>> = HashMap::new();
    let mut facts_by_path: HashMap<Utf8PathBuf, DocumentFacts> = HashMap::new();

    for file in files {
        by_path.insert(file.path.as_str().to_string(), file.path.clone());
        by_path_lower.insert(file.path.as_str().to_lowercase(), file.path.clone());
    }

    for document in documents.iter() {
        by_stem
            .entry(document.stem.to_lowercase())
            .or_default()
            .push(document.path.clone());
        for alias in &document.aliases {
            // alias is already lowercased upstream (parse_aliases in graph::aliases)
            by_alias
                .entry(alias.clone())
                .or_default()
                .push(document.path.clone());
        }
        facts_by_path.insert(
            document.path.clone(),
            DocumentFacts {
                heading_slugs: document
                    .headings
                    .iter()
                    .map(|heading| heading.slug.clone())
                    .collect(),
                block_ids: document.block_ids.clone(),
            },
        );
    }

    for document in documents.iter_mut() {
        for link in &mut document.links {
            let candidates = match link.kind {
                LinkKind::Markdown => {
                    resolve_markdown_link(&document.path, &link.target, &by_path, &by_path_lower)
                }
                LinkKind::Embed => {
                    if link.target.is_empty() && (link.anchor.is_some() || link.block_ref.is_some())
                    {
                        vec![document.path.clone()]
                    } else {
                        resolve_embed_link(
                            &document.path,
                            &link.target,
                            &by_path,
                            &by_path_lower,
                            &by_stem,
                            &by_alias,
                        )
                    }
                }
                LinkKind::Wikilink => {
                    if link.target.is_empty() && (link.anchor.is_some() || link.block_ref.is_some())
                    {
                        vec![document.path.clone()]
                    } else {
                        resolve_wikilink(
                            &link.target,
                            &by_path,
                            &by_path_lower,
                            &by_stem,
                            &by_alias,
                        )
                    }
                }
            };

            match candidates.as_slice() {
                [single] => {
                    link.resolved_path = Some(single.clone());
                    link.candidates = Vec::new();
                    validate_resolved_reference(link, single, &facts_by_path);
                }
                [] => {
                    link.status = LinkStatus::Unresolved;
                    link.resolved_path = None;
                    link.unresolved_reason = Some(UnresolvedReason::TargetMissing);
                    link.candidates = Vec::new();
                }
                many => {
                    link.status = LinkStatus::Ambiguous;
                    link.resolved_path = None;
                    link.unresolved_reason = Some(UnresolvedReason::Ambiguous);
                    link.candidates = many.to_vec();
                }
            }
        }
    }
}

#[derive(Clone)]
struct DocumentFacts {
    heading_slugs: Vec<String>,
    block_ids: Vec<String>,
}

fn validate_resolved_reference(
    link: &mut Link,
    target_path: &Utf8PathBuf,
    facts_by_path: &HashMap<Utf8PathBuf, DocumentFacts>,
) {
    let Some(facts) = facts_by_path.get(target_path) else {
        link.status = LinkStatus::Resolved;
        link.unresolved_reason = None;
        return;
    };

    if let Some(anchor) = &link.anchor {
        let anchor_slug = slugify(anchor);
        if !facts.heading_slugs.iter().any(|slug| slug == &anchor_slug) {
            link.status = LinkStatus::Unresolved;
            link.unresolved_reason = Some(UnresolvedReason::AnchorMissing);
            return;
        }
    }

    if let Some(block_ref) = &link.block_ref {
        if !facts.block_ids.iter().any(|block_id| block_id == block_ref) {
            link.status = LinkStatus::Unresolved;
            link.unresolved_reason = Some(UnresolvedReason::BlockRefMissing);
            return;
        }
    }

    link.status = LinkStatus::Resolved;
    link.unresolved_reason = None;
}

fn resolve_markdown_link(
    source_path: &Utf8Path,
    target: &str,
    by_path: &HashMap<String, Utf8PathBuf>,
    by_path_lower: &HashMap<String, Utf8PathBuf>,
) -> Vec<Utf8PathBuf> {
    let base = source_path.parent().unwrap_or_else(|| Utf8Path::new(""));
    resolve_path_like_target(base, target, by_path, by_path_lower)
}

fn resolve_embed_link(
    source_path: &Utf8Path,
    target: &str,
    by_path: &HashMap<String, Utf8PathBuf>,
    by_path_lower: &HashMap<String, Utf8PathBuf>,
    by_stem: &HashMap<String, Vec<Utf8PathBuf>>,
    by_alias: &HashMap<String, Vec<Utf8PathBuf>>,
) -> Vec<Utf8PathBuf> {
    let base = source_path.parent().unwrap_or_else(|| Utf8Path::new(""));
    let base_matches = resolve_path_like_target(base, target, by_path, by_path_lower);
    if !base_matches.is_empty() {
        return base_matches;
    }

    let root_matches = resolve_path_like_target(Utf8Path::new(""), target, by_path, by_path_lower);
    if !root_matches.is_empty() {
        return root_matches;
    }

    resolve_wikilink(target, by_path, by_path_lower, by_stem, by_alias)
}

fn resolve_wikilink(
    target: &str,
    by_path: &HashMap<String, Utf8PathBuf>,
    by_path_lower: &HashMap<String, Utf8PathBuf>,
    by_stem: &HashMap<String, Vec<Utf8PathBuf>>,
    by_alias: &HashMap<String, Vec<Utf8PathBuf>>,
) -> Vec<Utf8PathBuf> {
    if target.contains('/') {
        let path_matches =
            resolve_path_like_target(Utf8Path::new(""), target, by_path, by_path_lower);
        if !path_matches.is_empty() {
            return path_matches;
        }
    }

    // Derive the stem key WITHOUT `Path::file_stem`, which truncates at the LAST
    // dot and mangles dotted stems (`v0.40.0` -> `v0.40`, `periodic-0.4-review`
    // -> `periodic-0`), stranding otherwise-resolvable wikilinks (NRN-123).
    // Replicate file_stem's two useful effects deliberately: take the final path
    // component (so a stale `dir/name` target still falls back to the `name`
    // stem, keeping such links in the move/delete cascade set), then strip only a
    // literal `.md` (never an arbitrary extension). Lowercase once; by_stem/
    // by_alias keys are lowercased at construction.
    let target_lower = target.to_lowercase();
    let last_component = target_lower.rsplit('/').next().unwrap_or(&target_lower);
    let stem = last_component.strip_suffix(".md").unwrap_or(last_component);
    let stem_matches: Vec<Utf8PathBuf> = by_stem.get(stem).cloned().unwrap_or_default();
    if !stem_matches.is_empty() {
        return stem_matches;
    }

    // Fallback: consult aliases only when stem returned ∅.
    by_alias.get(stem).cloned().unwrap_or_default()
}

fn resolve_path_like_target(
    base: &Utf8Path,
    target: &str,
    by_path: &HashMap<String, Utf8PathBuf>,
    by_path_lower: &HashMap<String, Utf8PathBuf>,
) -> Vec<Utf8PathBuf> {
    let candidate = normalize_relative(base, target);
    if let Some(path) = by_path.get(candidate.as_str()) {
        return vec![path.clone()];
    }
    if let Some(path) = by_path_lower.get(&candidate.as_str().to_lowercase()) {
        return vec![path.clone()];
    }

    if candidate.extension().is_none() {
        let with_markdown_extension = candidate.with_extension("md");
        if let Some(path) = by_path.get(with_markdown_extension.as_str()) {
            return vec![path.clone()];
        }
        if let Some(path) = by_path_lower.get(&with_markdown_extension.as_str().to_lowercase()) {
            return vec![path.clone()];
        }
    }

    Vec::new()
}

fn normalize_relative(base: &Utf8Path, target: &str) -> Utf8PathBuf {
    let joined = base.join(target);
    let mut normalized = Utf8PathBuf::new();
    for component in joined.as_std_path().components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part.to_string_lossy().as_ref()),
            Component::RootDir | Component::Prefix(_) => {}
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Document, Heading, Link, LinkKind, LinkStatus, VaultFile};

    fn make_file(path: &str) -> VaultFile {
        VaultFile {
            path: path.into(),
            stem: Utf8Path::new(path).file_stem().unwrap().to_string(),
            extension: Utf8Path::new(path).extension().map(str::to_string),
            hash: None,
        }
    }

    fn make_document(path: &str) -> Document {
        Document {
            path: path.into(),
            stem: Utf8Path::new(path).file_stem().unwrap().to_string(),
            hash: String::new(),
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

    fn make_wikilink(source: &str, target: &str) -> Link {
        Link {
            source_path: source.into(),
            raw: format!("[[{target}]]"),
            kind: LinkKind::Wikilink,
            target: target.to_string(),
            label: None,
            anchor: None,
            block_ref: None,
            source_span: None,
            source_context: None,
            resolved_path: None,
            unresolved_reason: None,
            candidates: vec![],
            status: LinkStatus::Unresolved,
        }
    }

    #[test]
    fn unique_stem_wikilink_resolves() {
        let files = vec![make_file("a.md"), make_file("b.md")];
        let mut documents = vec![make_document("a.md"), make_document("b.md")];
        documents[0].links.push(make_wikilink("a.md", "b"));
        resolve_links(&files, &mut documents);
        let link = &documents[0].links[0];
        assert_eq!(link.status, LinkStatus::Resolved);
        assert_eq!(link.resolved_path, Some("b.md".into()));
    }

    #[test]
    fn ambiguous_stem_wikilink_returns_ambiguous_with_candidates() {
        let files = vec![
            make_file("a.md"),
            make_file("dir/a.md"),
            make_file("src.md"),
        ];
        let mut documents = vec![
            make_document("a.md"),
            make_document("dir/a.md"),
            make_document("src.md"),
        ];
        documents[2].links.push(make_wikilink("src.md", "a"));
        resolve_links(&files, &mut documents);
        let link = &documents[2].links[0];
        assert_eq!(link.status, LinkStatus::Ambiguous);
        assert_eq!(link.candidates.len(), 2);
    }

    #[test]
    fn missing_target_wikilink_returns_target_missing() {
        let files = vec![make_file("a.md")];
        let mut documents = vec![make_document("a.md")];
        documents[0].links.push(make_wikilink("a.md", "missing"));
        resolve_links(&files, &mut documents);
        let link = &documents[0].links[0];
        assert_eq!(link.status, LinkStatus::Unresolved);
        assert_eq!(
            link.unresolved_reason,
            Some(UnresolvedReason::TargetMissing)
        );
    }

    #[test]
    fn dotted_stem_wikilink_resolves() {
        // NRN-123: Path::file_stem truncates at the last dot, mangling dotted
        // stems like `v0.40.0` -> `v0.40`. The wikilink must resolve to the file.
        let files = vec![make_file("notes/v0.40.0.md"), make_file("a.md")];
        let mut documents = vec![make_document("notes/v0.40.0.md"), make_document("a.md")];
        documents[1].links.push(make_wikilink("a.md", "v0.40.0"));
        resolve_links(&files, &mut documents);
        let link = &documents[1].links[0];
        assert_eq!(link.status, LinkStatus::Resolved);
        assert_eq!(link.resolved_path, Some("notes/v0.40.0.md".into()));
    }

    #[test]
    fn dotted_stem_midstring_wikilink_resolves() {
        // `periodic-0.4-review` would be chopped to `periodic-0` by file_stem.
        let files = vec![make_file("logs/periodic-0.4-review.md"), make_file("a.md")];
        let mut documents = vec![
            make_document("logs/periodic-0.4-review.md"),
            make_document("a.md"),
        ];
        documents[1]
            .links
            .push(make_wikilink("a.md", "periodic-0.4-review"));
        resolve_links(&files, &mut documents);
        let link = &documents[1].links[0];
        assert_eq!(link.status, LinkStatus::Resolved);
        assert_eq!(
            link.resolved_path,
            Some("logs/periodic-0.4-review.md".into())
        );
    }

    #[test]
    fn path_qualified_wikilink_falls_back_to_final_component_stem() {
        // A `dir/name` wikilink whose exact path is stale must still resolve via
        // the final component's stem — the dotted-stem fix must NOT drop the
        // path-qualified stem fallback that file_stem previously provided
        // (otherwise these links silently leave the move/delete cascade set).
        let files = vec![make_file("other/note.md"), make_file("src.md")];
        let mut documents = vec![make_document("other/note.md"), make_document("src.md")];
        documents[1]
            .links
            .push(make_wikilink("src.md", "folder/note"));
        resolve_links(&files, &mut documents);
        let link = &documents[1].links[0];
        assert_eq!(link.status, LinkStatus::Resolved);
        assert_eq!(link.resolved_path, Some("other/note.md".into()));
    }

    #[test]
    fn dotted_alias_wikilink_resolves() {
        // The by_alias lookup key was also mangled by file_stem; a dotted alias
        // value must resolve now (was `v0.40` before, missing the alias).
        let files = vec![make_file("release-notes.md"), make_file("a.md")];
        let mut documents = vec![make_document("release-notes.md"), make_document("a.md")];
        documents[0].aliases = vec!["v0.40.0".to_string()];
        documents[1].links.push(make_wikilink("a.md", "v0.40.0"));
        resolve_links(&files, &mut documents);
        let link = &documents[1].links[0];
        assert_eq!(link.status, LinkStatus::Resolved);
        assert_eq!(link.resolved_path, Some("release-notes.md".into()));
    }

    #[test]
    fn ambiguous_dotted_stem_wikilink_is_ambiguous() {
        // A dotted stem shared by two docs must be Ambiguous, not silently
        // resolved to one — the fix routes dotted stems into the multi-candidate
        // branch, so pin that it still reports ambiguity.
        let files = vec![
            make_file("x/v0.40.0.md"),
            make_file("y/v0.40.0.md"),
            make_file("src.md"),
        ];
        let mut documents = vec![
            make_document("x/v0.40.0.md"),
            make_document("y/v0.40.0.md"),
            make_document("src.md"),
        ];
        documents[2].links.push(make_wikilink("src.md", "v0.40.0"));
        resolve_links(&files, &mut documents);
        let link = &documents[2].links[0];
        assert_eq!(link.status, LinkStatus::Ambiguous);
        assert_eq!(link.candidates.len(), 2);
    }

    #[test]
    fn non_md_extension_target_does_not_cross_resolve_to_md() {
        // NRN-123 intent: strip ONLY `.md`. A `[[diagram.png]]` wikilink must NOT
        // resolve to a `diagram.md` doc (file_stem used to strip any extension,
        // a false positive). This pins the deliberate behavior change.
        let files = vec![make_file("diagram.md"), make_file("a.md")];
        let mut documents = vec![make_document("diagram.md"), make_document("a.md")];
        documents[1]
            .links
            .push(make_wikilink("a.md", "diagram.png"));
        resolve_links(&files, &mut documents);
        let link = &documents[1].links[0];
        assert_eq!(link.status, LinkStatus::Unresolved);
    }

    #[test]
    fn wikilink_with_md_suffix_still_strips_extension() {
        // Regression guard: a target that DOES carry a trailing `.md` must still
        // resolve by stem after the file_stem->strip_suffix change.
        let files = vec![make_file("a.md"), make_file("b.md")];
        let mut documents = vec![make_document("a.md"), make_document("b.md")];
        documents[0].links.push(make_wikilink("a.md", "b.md"));
        resolve_links(&files, &mut documents);
        let link = &documents[0].links[0];
        assert_eq!(link.status, LinkStatus::Resolved);
        assert_eq!(link.resolved_path, Some("b.md".into()));
    }

    #[test]
    fn case_only_duplicate_filenames_silently_overwrite_in_path_lower() {
        // Documents the known fragility: by_path_lower has only one entry for two
        // paths that differ only in case. The second insert wins. This test pins
        // the current behavior; future work might emit a diagnostic.
        let files = vec![make_file("Foo.md"), make_file("foo.md")];
        let mut documents = vec![make_document("Foo.md"), make_document("foo.md")];
        documents[0].links.push(make_wikilink("Foo.md", "FOO"));
        resolve_links(&files, &mut documents);
        let link = &documents[0].links[0];
        assert!(matches!(
            link.status,
            LinkStatus::Resolved | LinkStatus::Ambiguous
        ));
    }

    #[test]
    fn embed_same_note_block_ref_resolves_to_self() {
        let files = vec![make_file("a.md")];
        let mut documents = vec![make_document("a.md")];
        documents[0].block_ids.push("block-1".to_string());
        let mut link = make_wikilink("a.md", "");
        link.kind = LinkKind::Embed;
        link.block_ref = Some("block-1".to_string());
        documents[0].links.push(link);
        resolve_links(&files, &mut documents);
        let link = &documents[0].links[0];
        assert_eq!(link.status, LinkStatus::Resolved);
        assert_eq!(link.resolved_path, Some("a.md".into()));
    }

    #[test]
    fn wikilink_with_missing_anchor_returns_anchor_missing() {
        let files = vec![make_file("a.md"), make_file("b.md")];
        let mut documents = vec![make_document("a.md"), make_document("b.md")];
        documents[1].headings.push(Heading {
            level: 1,
            text: "Existing".into(),
            slug: "existing".into(),
            source_span: None,
        });
        let mut link = make_wikilink("a.md", "b");
        link.anchor = Some("Missing".to_string());
        documents[0].links.push(link);
        resolve_links(&files, &mut documents);
        let link = &documents[0].links[0];
        assert_eq!(link.status, LinkStatus::Unresolved);
        assert_eq!(
            link.unresolved_reason,
            Some(UnresolvedReason::AnchorMissing)
        );
    }

    fn make_document_with_aliases(path: &str, aliases: Vec<&str>) -> Document {
        let mut doc = make_document(path);
        doc.aliases = aliases.into_iter().map(String::from).collect();
        doc
    }

    #[test]
    fn alias_only_resolves_when_stem_absent() {
        let files = vec![make_file("vault-memory.md"), make_file("notes.md")];
        let mut documents = vec![
            make_document_with_aliases("vault-memory.md", vec!["vault memory"]),
            make_document("notes.md"),
        ];
        documents[1]
            .links
            .push(make_wikilink("notes.md", "Vault Memory"));
        resolve_links(&files, &mut documents);
        let link = &documents[1].links[0];
        assert_eq!(link.status, LinkStatus::Resolved);
        assert_eq!(link.resolved_path, Some("vault-memory.md".into()));
    }

    #[test]
    fn stem_wins_over_alias() {
        // Doc-A has stem `foo`; Doc-B aliases `foo`. [[foo]] resolves to Doc-A via stem.
        let files = vec![make_file("foo.md"), make_file("bar.md")];
        let mut documents = vec![
            make_document("foo.md"),
            make_document_with_aliases("bar.md", vec!["foo"]),
        ];
        documents[0].links.push(make_wikilink("foo.md", "foo"));
        resolve_links(&files, &mut documents);
        let link = &documents[0].links[0];
        assert_eq!(link.status, LinkStatus::Resolved);
        assert_eq!(link.resolved_path, Some("foo.md".into()));
    }

    #[test]
    fn duplicate_aliases_yield_ambiguous() {
        // Two docs both claim alias `vault memory`; no stem matches. Resolution → Ambiguous.
        let files = vec![
            make_file("doc-a.md"),
            make_file("doc-b.md"),
            make_file("src.md"),
        ];
        let mut documents = vec![
            make_document_with_aliases("doc-a.md", vec!["vault memory"]),
            make_document_with_aliases("doc-b.md", vec!["vault memory"]),
            make_document("src.md"),
        ];
        documents[2]
            .links
            .push(make_wikilink("src.md", "Vault Memory"));
        resolve_links(&files, &mut documents);
        let link = &documents[2].links[0];
        assert_eq!(link.status, LinkStatus::Ambiguous);
        assert_eq!(link.candidates.len(), 2);
    }

    #[test]
    fn alias_target_with_anchor_validates_against_resolved_doc_headings() {
        let files = vec![make_file("vault-memory.md"), make_file("src.md")];
        let mut documents = vec![
            make_document_with_aliases("vault-memory.md", vec!["vault memory"]),
            make_document("src.md"),
        ];
        documents[0].headings.push(Heading {
            level: 1,
            text: "Architecture".into(),
            slug: "architecture".into(),
            source_span: None,
        });
        let mut link = make_wikilink("src.md", "Vault Memory");
        link.anchor = Some("Architecture".to_string());
        documents[1].links.push(link);
        resolve_links(&files, &mut documents);
        let link = &documents[1].links[0];
        assert_eq!(link.status, LinkStatus::Resolved);
    }

    #[test]
    fn alias_target_with_missing_anchor_emits_anchor_missing() {
        let files = vec![make_file("vault-memory.md"), make_file("src.md")];
        let mut documents = vec![
            make_document_with_aliases("vault-memory.md", vec!["vault memory"]),
            make_document("src.md"),
        ];
        let mut link = make_wikilink("src.md", "Vault Memory");
        link.anchor = Some("DoesNotExist".to_string());
        documents[1].links.push(link);
        resolve_links(&files, &mut documents);
        let link = &documents[1].links[0];
        assert_eq!(link.status, LinkStatus::Unresolved);
        assert_eq!(
            link.unresolved_reason,
            Some(UnresolvedReason::AnchorMissing)
        );
    }
}
