use std::collections::HashMap;
use std::path::Component;

use camino::{Utf8Path, Utf8PathBuf};
use vault_core::{Document, Link, LinkKind, LinkStatus, UnresolvedReason, VaultFile};

use crate::anchor::slugify;

pub fn resolve_links(files: &[VaultFile], documents: &mut [Document]) {
    let mut by_path: HashMap<String, Utf8PathBuf> = HashMap::new();
    let mut by_path_lower: HashMap<String, Utf8PathBuf> = HashMap::new();
    let mut by_stem: HashMap<String, Vec<Utf8PathBuf>> = HashMap::new();
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
                        )
                    }
                }
                LinkKind::Wikilink => {
                    if link.target.is_empty() && (link.anchor.is_some() || link.block_ref.is_some())
                    {
                        vec![document.path.clone()]
                    } else {
                        resolve_wikilink(&link.target, &by_path, &by_path_lower, &by_stem)
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

    resolve_wikilink(target, by_path, by_path_lower, by_stem)
}

fn resolve_wikilink(
    target: &str,
    by_path: &HashMap<String, Utf8PathBuf>,
    by_path_lower: &HashMap<String, Utf8PathBuf>,
    by_stem: &HashMap<String, Vec<Utf8PathBuf>>,
) -> Vec<Utf8PathBuf> {
    if target.contains('/') {
        let path_matches =
            resolve_path_like_target(Utf8Path::new(""), target, by_path, by_path_lower);
        if !path_matches.is_empty() {
            return path_matches;
        }
    }

    let stem = Utf8Path::new(target).file_stem().unwrap_or(target);
    by_stem
        .get(&stem.to_lowercase())
        .cloned()
        .unwrap_or_default()
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
