//! The reverse link index: which documents' links consult which lookup-table
//! entry — the structure that turns "these paths changed" into "these documents
//! must be re-resolved" without walking the vault's links.
//!
//! # Why a key, not a target
//!
//! Link resolution ([`super::resolve`]) is a pure function of four lookup
//! tables. Every entry a given link can read is named by a [`LookupKey`], and a
//! vault path OWNS a small, fixed set of keys ([`path_lookup_keys`]): the
//! lowercased path it occupies and the lowercased stem it contributes to. So a
//! link whose key set is disjoint from the changed paths' key sets CANNOT change
//! its answer — not because its target still exists, but because nothing it
//! reads moved.
//!
//! That is what makes the blast radius exact in both directions the naive
//! "re-resolve the touched document" shortcut gets wrong: a CREATE can resolve a
//! previously-dangling link somewhere else in the vault (the new path owns the
//! key that link was missing), and a DELETE can dangle one (the removed path
//! owned the key that link was hitting). Both are reverse-lookup hits.
//!
//! # Today: rebuilt per overlay call
//!
//! [`build`](ReverseLinkIndex::build) is called fresh on every
//! [`overlay_changed_paths`](crate::graph::overlay_changed_paths) invocation,
//! over the PRE-change document set — that is where the links a create can
//! newly satisfy, and a delete can newly dangle, still live. Building walks
//! every link once (O(total links) in the vault), but each link's own work is a
//! smaller constant than resolving it: computing its key set, not doing a
//! lookup against the resolution tables.
//!
//! `record` already does its work per document, one document's key set at a
//! time, independent of every other document's — that shape is what makes a
//! FUTURE warm index cheap (NRN-523: keep the index alive across calls and
//! replace one document's entries in place instead of rebuilding). There is no
//! warm index yet: no `remove`, no public per-document `update`, nothing
//! persists between calls today.
//!
//! Keys are computed WITHOUT short-circuiting the ladder, so the recorded set is
//! a superset of what any particular vault state makes the link read — an
//! over-approximation re-resolves a link needlessly, it never misses one.

use std::collections::{BTreeSet, HashMap};

use camino::{Utf8Path, Utf8PathBuf};

use super::resolve::{normalize_relative, wikilink_stem_key};
use crate::domain::{Document, Link, LinkKind};

/// One entry in the lookup tables link resolution consults.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) enum LookupKey {
    /// A vault-relative path, lowercased. One key covers both the exact and the
    /// case-insensitive path tables, because a file's presence writes and erases
    /// the two together.
    Path(String),
    /// A document stem, lowercased — a `by_stem` bucket.
    Stem(String),
}

/// The keys a vault path OWNS: the table entries its presence or absence
/// decides. A changed path can only flip links that consult one of these.
pub(crate) fn path_lookup_keys(path: &Utf8Path) -> Vec<LookupKey> {
    // Normalize through resolution's own function so a `./a.md` spelling of a
    // changed path owns the same key a link's `a.md` candidate derives.
    let normalized = normalize_relative(Utf8Path::new(""), path.as_str());
    let mut keys = vec![LookupKey::Path(normalized.as_str().to_lowercase())];
    if let Some(stem) = normalized.file_stem() {
        keys.push(LookupKey::Stem(stem.to_lowercase()));
    }
    keys
}

/// Every table entry the resolution ladder can read for one link, computed
/// WITHOUT short-circuiting — the ladder stops at its first hit, but which rung
/// hits depends on the vault state this set is meant to be invariant to.
///
/// Mirrors [`super::resolve`]'s ladder rung for rung; the incremental-equals-
/// rebuild property test is what holds the two in step.
pub(crate) fn link_lookup_keys(source_path: &Utf8Path, link: &Link) -> Vec<LookupKey> {
    let mut keys = Vec::new();
    let base = source_path.parent().unwrap_or_else(|| Utf8Path::new(""));
    let self_reference =
        link.target.is_empty() && (link.anchor.is_some() || link.block_ref.is_some());

    match link.kind {
        // A Markdown link never takes the self-reference branch: an empty target
        // resolves path-like against the source document's own directory.
        LinkKind::Markdown => push_path_like_keys(base, &link.target, &mut keys),
        LinkKind::Embed if self_reference => keys.push(source_path_key(source_path)),
        LinkKind::Embed => {
            push_path_like_keys(base, &link.target, &mut keys);
            push_path_like_keys(Utf8Path::new(""), &link.target, &mut keys);
            push_wikilink_keys(&link.target, &mut keys);
        }
        LinkKind::Wikilink if self_reference => keys.push(source_path_key(source_path)),
        LinkKind::Wikilink => push_wikilink_keys(&link.target, &mut keys),
    }
    keys
}

fn source_path_key(source_path: &Utf8Path) -> LookupKey {
    LookupKey::Path(source_path.as_str().to_lowercase())
}

fn push_path_like_keys(base: &Utf8Path, target: &str, keys: &mut Vec<LookupKey>) {
    let candidate = normalize_relative(base, target);
    keys.push(LookupKey::Path(candidate.as_str().to_lowercase()));
    if candidate.extension().is_none() {
        keys.push(LookupKey::Path(
            candidate.with_extension("md").as_str().to_lowercase(),
        ));
    }
}

fn push_wikilink_keys(target: &str, keys: &mut Vec<LookupKey>) {
    if target.contains('/') {
        push_path_like_keys(Utf8Path::new(""), target, keys);
    }
    keys.push(LookupKey::Stem(wikilink_stem_key(target)));
}

/// Lookup key → the documents whose links read it.
#[derive(Debug, Default)]
pub(crate) struct ReverseLinkIndex {
    sources_by_key: HashMap<LookupKey, Vec<Utf8PathBuf>>,
}

impl ReverseLinkIndex {
    /// Index every link of every document. O(links), with one key buffer reused
    /// across the whole set rather than a fresh collection per document.
    pub(crate) fn build(documents: &[Document]) -> Self {
        // Capacity is link-derived, not document-derived: each link contributes
        // up to 5 distinct keys (see `link_lookup_keys`; a slash-bearing
        // extensionless embed records base+root paths, their `.md` variants,
        // and the stem), so `documents.len()` alone
        // undersizes the map on any vault where documents carry more than a
        // handful of links each.
        let link_count: usize = documents.iter().map(|document| document.links.len()).sum();
        let mut index = Self {
            sources_by_key: HashMap::with_capacity(link_count),
        };
        let mut keys = Vec::new();
        for document in documents {
            index.record(document, &mut keys);
        }
        index
    }

    /// Record one document's links — the per-document unit of update. `keys` is
    /// a caller-owned scratch buffer, cleared on entry.
    fn record(&mut self, document: &Document, keys: &mut Vec<LookupKey>) {
        keys.clear();
        for link in &document.links {
            for key in link_lookup_keys(&document.path, link) {
                // Key sets per document are small; a linear dedupe beats a
                // per-document hash set allocation at this size.
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
        }
        for key in keys.drain(..) {
            self.sources_by_key
                .entry(key)
                .or_default()
                .push(document.path.clone());
        }
    }

    /// The documents whose links read a key owned by any of `changed_paths` —
    /// the blast radius of that change set.
    pub(crate) fn sources_for_paths<'a>(
        &self,
        changed_paths: impl IntoIterator<Item = &'a Utf8Path>,
    ) -> BTreeSet<Utf8PathBuf> {
        let mut affected = BTreeSet::new();
        for path in changed_paths {
            for key in path_lookup_keys(path) {
                if let Some(sources) = self.sources_by_key.get(&key) {
                    affected.extend(sources.iter().cloned());
                }
            }
        }
        affected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Document, LinkStatus};

    fn document(path: &str, targets: &[(&str, LinkKind)]) -> Document {
        let links = targets
            .iter()
            .map(|(target, kind)| Link {
                source_path: path.into(),
                raw: format!("[[{target}]]"),
                kind: kind.clone(),
                target: (*target).to_string(),
                label: None,
                anchor: None,
                block_ref: None,
                source_span: None,
                source_context: None,
                resolved_path: None,
                unresolved_reason: None,
                candidates: vec![],
                status: LinkStatus::Unresolved,
            })
            .collect();
        Document {
            path: path.into(),
            stem: Utf8Path::new(path).file_stem().unwrap().to_string(),
            hash: String::new(),
            frontmatter: None,
            head_text: String::new(),
            body_text: String::new(),
            headings: vec![],
            block_ids: vec![],
            links,
            diagnostics: vec![],
            aliases: vec![],
            alias_malformed: vec![],
        }
    }

    #[test]
    fn a_dangling_wikilink_is_found_by_the_path_that_would_satisfy_it() {
        // The create case: `beta.md` does not exist yet, so `a.md`'s link is
        // dangling — but adding it must re-resolve `a.md`, not just `beta.md`.
        let docs = vec![document("a.md", &[("beta", LinkKind::Wikilink)])];
        let index = ReverseLinkIndex::build(&docs);
        let affected = index.sources_for_paths([Utf8Path::new("beta.md")]);
        assert_eq!(affected, BTreeSet::from([Utf8PathBuf::from("a.md")]));
    }

    #[test]
    fn a_relative_markdown_link_is_found_by_its_normalized_target() {
        let docs = vec![document("dir/a.md", &[("../top.md", LinkKind::Markdown)])];
        let index = ReverseLinkIndex::build(&docs);
        assert_eq!(
            index.sources_for_paths([Utf8Path::new("top.md")]),
            BTreeSet::from([Utf8PathBuf::from("dir/a.md")])
        );
        assert!(index
            .sources_for_paths([Utf8Path::new("dir/top.md")])
            .is_empty());
    }

    #[test]
    fn keys_are_case_insensitive_on_both_sides() {
        let docs = vec![document("a.md", &[("Beta", LinkKind::Wikilink)])];
        let index = ReverseLinkIndex::build(&docs);
        assert_eq!(
            index.sources_for_paths([Utf8Path::new("notes/BETA.md")]),
            BTreeSet::from([Utf8PathBuf::from("a.md")])
        );
    }

    #[test]
    fn a_changed_path_spelled_with_a_cur_dir_component_owns_the_same_keys() {
        let docs = vec![document("a.md", &[("beta", LinkKind::Wikilink)])];
        let index = ReverseLinkIndex::build(&docs);
        assert_eq!(
            index.sources_for_paths([Utf8Path::new("./beta.md")]),
            BTreeSet::from([Utf8PathBuf::from("a.md")])
        );
    }

    #[test]
    fn every_document_reading_a_key_is_reported() {
        let docs = vec![
            document("a.md", &[("beta", LinkKind::Wikilink)]),
            document("b.md", &[("beta", LinkKind::Wikilink)]),
            document("c.md", &[("gamma", LinkKind::Wikilink)]),
        ];
        let index = ReverseLinkIndex::build(&docs);
        assert_eq!(
            index.sources_for_paths([Utf8Path::new("beta.md")]),
            BTreeSet::from([Utf8PathBuf::from("a.md"), Utf8PathBuf::from("b.md")])
        );
        assert!(index
            .sources_for_paths([Utf8Path::new("unrelated.md")])
            .is_empty());
    }

    #[test]
    fn a_document_with_no_links_records_nothing() {
        let index = ReverseLinkIndex::build(&[document("a.md", &[])]);
        assert!(index.sources_by_key.is_empty());
    }

    #[test]
    fn an_embed_records_every_rung_it_could_read() {
        // base-relative path, root-relative path, and the stem bucket.
        let docs = vec![document("dir/a.md", &[("shared", LinkKind::Embed)])];
        let index = ReverseLinkIndex::build(&docs);
        for candidate in ["dir/shared.md", "shared.md", "elsewhere/shared.md"] {
            assert_eq!(
                index.sources_for_paths([Utf8Path::new(candidate)]),
                BTreeSet::from([Utf8PathBuf::from("dir/a.md")]),
                "an embed must be re-resolved when {candidate} changes"
            );
        }
    }
}
