//! Link resolution — matching a parsed [`Link`]'s target to a document in the
//! vault, deciding resolved / unresolved / ambiguous status.
//!
//! This is the semantic half of the link model (the lexical half — token and
//! anchor syntax — is `norn_frontmatter`). Resolution is external contract: which
//! path a target resolves to, when it is ambiguous, and which anchors/blocks
//! validate are observable behavior.
//!
//! The resolution ladder is **path → stem ONLY** (NRN-455): aliases do NOT
//! participate. A wikilink whose target matches only a document's `aliases`
//! frontmatter entry is `Unresolved`/`TargetMissing`, not resolved. The `aliases`
//! field remains ordinary queryable frontmatter; a dangling link that uniquely
//! matches one doc's alias is surfaced as a deterministic `repair` hint (rewrite
//! to the canonical stem link), not resolved here.

use std::collections::{BTreeSet, HashMap};
use std::path::Component;

use crate::domain::{Document, Link, LinkKind, LinkStatus, UnresolvedReason, VaultFile};
use camino::{Utf8Path, Utf8PathBuf};
use norn_frontmatter::heading::slugify;

// Test-only, PER-THREAD tally of links actually re-resolved on the current
// thread. A whole-graph resolve touches every link in the vault; a bounded
// re-resolution touches the blast radius. The incremental-cost guard resets it,
// drives one create, and reads the count to prove re-resolution scope tracks the
// affected set, not the vault. Thread-local so the parallel test runner's other
// resolvers never pollute the measurement.
#[cfg(test)]
thread_local! {
    static LINKS_RESOLVED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Reset the current thread's link-resolution tally (test-only).
#[cfg(test)]
pub(crate) fn links_resolved_reset() {
    LINKS_RESOLVED.with(|count| count.set(0));
}

/// Read the current thread's link-resolution tally (test-only).
#[cfg(test)]
pub(crate) fn links_resolved_count() -> usize {
    LINKS_RESOLVED.with(|count| count.get())
}

/// Resolve every link on every document in place against the vault's file and
/// document tables. Populates each link's `status`, `resolved_path`,
/// `unresolved_reason`, and `candidates`. This is the whole-graph entry point —
/// cold start and full derivation; a maintenance path that knows its blast
/// radius calls [`resolve_links_within`] instead.
pub fn resolve_links(files: &[VaultFile], documents: &mut [Document]) {
    resolve_links_within(files, documents, None);
}

/// Resolve links for the documents named by `scope` (all of them when `None`),
/// against the tables derived from the WHOLE `files`/`documents` pair.
///
/// Resolution semantics stay whole-graph: every candidate lookup sees every
/// document, so a target added anywhere is visible here. What `scope` bounds is
/// which links are re-derived — the caller asserts that every other link's
/// answer is unchanged. [`ReverseLinkIndex`](super::reverse::ReverseLinkIndex)
/// is what lets a maintenance path make that assertion soundly.
///
/// Resolution runs in two passes so it can borrow the documents it reads instead
/// of copying them: pass one computes every in-scope link's outcome while
/// `documents` is borrowed immutably (the lookup tables point INTO it — no
/// per-document heading/block-id copies), pass two writes the outcomes back.
pub(crate) fn resolve_links_within(
    files: &[VaultFile],
    documents: &mut [Document],
    scope: Option<&BTreeSet<Utf8PathBuf>>,
) {
    let planned = {
        let tables = ResolutionTables::build(files, documents);
        let mut planned: Vec<(usize, Vec<LinkResolution>)> = Vec::new();
        for (position, document) in documents.iter().enumerate() {
            if scope.is_some_and(|scope| !scope.contains(&document.path)) {
                continue;
            }
            if document.links.is_empty() {
                continue;
            }
            #[cfg(test)]
            LINKS_RESOLVED.with(|count| count.set(count.get() + document.links.len()));
            let outcomes = document
                .links
                .iter()
                .map(|link| tables.resolve(&document.path, link))
                .collect();
            planned.push((position, outcomes));
        }
        planned
    };

    for (position, outcomes) in planned {
        for (link, outcome) in documents[position].links.iter_mut().zip(outcomes) {
            outcome.write_into(link);
        }
    }
}

/// One link's resolution outcome, owned so it can outlive the immutable borrow
/// of the documents it was derived from.
struct LinkResolution {
    status: LinkStatus,
    resolved_path: Option<Utf8PathBuf>,
    unresolved_reason: Option<UnresolvedReason>,
    candidates: Vec<Utf8PathBuf>,
}

impl LinkResolution {
    fn write_into(self, link: &mut Link) {
        link.status = self.status;
        link.resolved_path = self.resolved_path;
        link.unresolved_reason = self.unresolved_reason;
        link.candidates = self.candidates;
    }
}

/// The lookup tables the resolution ladder consults, borrowed from the graph
/// they describe. Every value is a reference into `files`/`documents`, so
/// building the tables costs hashing and no content copies.
struct ResolutionTables<'a> {
    by_path: HashMap<&'a str, &'a Utf8Path>,
    by_path_lower: HashMap<String, &'a Utf8Path>,
    by_stem: HashMap<String, Vec<&'a Utf8Path>>,
    by_document_path: HashMap<&'a Utf8Path, &'a Document>,
}

impl<'a> ResolutionTables<'a> {
    fn build(files: &'a [VaultFile], documents: &'a [Document]) -> Self {
        let mut by_path: HashMap<&'a str, &'a Utf8Path> = HashMap::with_capacity(files.len());
        let mut by_path_lower: HashMap<String, &'a Utf8Path> = HashMap::with_capacity(files.len());
        for file in files {
            by_path.insert(file.path.as_str(), file.path.as_path());
            by_path_lower.insert(file.path.as_str().to_lowercase(), file.path.as_path());
        }

        let mut by_stem: HashMap<String, Vec<&'a Utf8Path>> = HashMap::new();
        let mut by_document_path: HashMap<&'a Utf8Path, &'a Document> =
            HashMap::with_capacity(documents.len());
        for document in documents {
            by_stem
                .entry(document.stem.to_lowercase())
                .or_default()
                .push(document.path.as_path());
            by_document_path.insert(document.path.as_path(), document);
        }

        Self {
            by_path,
            by_path_lower,
            by_stem,
            by_document_path,
        }
    }

    fn resolve(&self, source_path: &Utf8Path, link: &Link) -> LinkResolution {
        let candidates = match link.kind {
            LinkKind::Markdown => self.resolve_markdown_link(source_path, &link.target),
            LinkKind::Embed => {
                if is_self_reference(link) {
                    vec![source_path]
                } else {
                    self.resolve_embed_link(source_path, &link.target)
                }
            }
            LinkKind::Wikilink => {
                if is_self_reference(link) {
                    vec![source_path]
                } else {
                    self.resolve_wikilink(&link.target)
                }
            }
        };

        match candidates.as_slice() {
            [single] => self.resolved_reference(link, single),
            [] => LinkResolution {
                status: LinkStatus::Unresolved,
                resolved_path: None,
                unresolved_reason: Some(UnresolvedReason::TargetMissing),
                candidates: Vec::new(),
            },
            many => LinkResolution {
                status: LinkStatus::Ambiguous,
                resolved_path: None,
                unresolved_reason: Some(UnresolvedReason::Ambiguous),
                candidates: many.iter().map(|path| path.to_path_buf()).collect(),
            },
        }
    }

    fn resolved_reference(&self, link: &Link, target_path: &Utf8Path) -> LinkResolution {
        let resolved = LinkResolution {
            status: LinkStatus::Resolved,
            resolved_path: Some(target_path.to_path_buf()),
            unresolved_reason: None,
            candidates: Vec::new(),
        };
        let Some(target) = self.by_document_path.get(target_path) else {
            return resolved;
        };

        if let Some(anchor) = &link.anchor {
            let anchor_slug = slugify(anchor);
            if !target
                .headings
                .iter()
                .any(|heading| heading.slug == anchor_slug)
            {
                return LinkResolution {
                    status: LinkStatus::Unresolved,
                    unresolved_reason: Some(UnresolvedReason::AnchorMissing),
                    ..resolved
                };
            }
        }

        if let Some(block_ref) = &link.block_ref {
            if !target
                .block_ids
                .iter()
                .any(|block_id| block_id == block_ref)
            {
                return LinkResolution {
                    status: LinkStatus::Unresolved,
                    unresolved_reason: Some(UnresolvedReason::BlockRefMissing),
                    ..resolved
                };
            }
        }

        resolved
    }

    fn resolve_markdown_link(&self, source_path: &Utf8Path, target: &str) -> Vec<&'a Utf8Path> {
        let base = source_path.parent().unwrap_or_else(|| Utf8Path::new(""));
        self.resolve_path_like_target(base, target)
    }

    fn resolve_embed_link(&self, source_path: &Utf8Path, target: &str) -> Vec<&'a Utf8Path> {
        let base = source_path.parent().unwrap_or_else(|| Utf8Path::new(""));
        let base_matches = self.resolve_path_like_target(base, target);
        if !base_matches.is_empty() {
            return base_matches;
        }

        let root_matches = self.resolve_path_like_target(Utf8Path::new(""), target);
        if !root_matches.is_empty() {
            return root_matches;
        }

        self.resolve_wikilink(target)
    }

    fn resolve_wikilink(&self, target: &str) -> Vec<&'a Utf8Path> {
        if target.contains('/') {
            let path_matches = self.resolve_path_like_target(Utf8Path::new(""), target);
            if !path_matches.is_empty() {
                return path_matches;
            }
        }

        // The ladder ends at stem (NRN-455): aliases do NOT participate in
        // resolution. A target that matches only an `aliases` entry returns ∅
        // here (dangling), and `repair` offers the deterministic
        // rewrite-to-stem hint instead.
        self.by_stem
            .get(&wikilink_stem_key(target))
            .cloned()
            .unwrap_or_default()
    }

    fn resolve_path_like_target(&self, base: &Utf8Path, target: &str) -> Vec<&'a Utf8Path> {
        let candidate = normalize_relative(base, target);
        if let Some(path) = self.lookup_path(&candidate) {
            return vec![path];
        }
        if candidate.extension().is_none() {
            if let Some(path) = self.lookup_path(&candidate.with_extension("md")) {
                return vec![path];
            }
        }
        Vec::new()
    }

    fn lookup_path(&self, candidate: &Utf8Path) -> Option<&'a Utf8Path> {
        self.by_path.get(candidate.as_str()).copied().or_else(|| {
            self.by_path_lower
                .get(&candidate.as_str().to_lowercase())
                .copied()
        })
    }
}

/// An empty target carrying an anchor or a block ref points at the source
/// document itself.
fn is_self_reference(link: &Link) -> bool {
    link.target.is_empty() && (link.anchor.is_some() || link.block_ref.is_some())
}

/// The `by_stem` bucket key a wikilink target derives.
///
/// Derived WITHOUT `Path::file_stem`, which truncates at the LAST dot and
/// mangles dotted stems (`v0.40.0` -> `v0.40`, `periodic-0.4-review` ->
/// `periodic-0`), stranding otherwise-resolvable wikilinks (NRN-123). Replicate
/// file_stem's two useful effects deliberately: take the final path component
/// (so a stale `dir/name` target still falls back to the `name` stem, keeping
/// such links in the move/delete cascade set), then strip only a literal `.md`
/// (never an arbitrary extension). Lowercase once; `by_stem` keys are lowercased
/// at construction.
pub(super) fn wikilink_stem_key(target: &str) -> String {
    let target_lower = target.to_lowercase();
    let last_component = target_lower.rsplit('/').next().unwrap_or(&target_lower);
    last_component
        .strip_suffix(".md")
        .unwrap_or(last_component)
        .to_string()
}

pub(super) fn normalize_relative(base: &Utf8Path, target: &str) -> Utf8PathBuf {
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
            head_text: String::new(),
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
    fn markdown_link_block_ref_validates_against_target_block_ids() {
        // NRN-356: a Markdown `[x](b.md#^blk1)` now carries a block_ref (not an
        // anchor), so resolution validates it against the target's block-ids and
        // resolves when the id exists (rather than slugifying `^blk1` as a
        // heading anchor and reporting anchor-missing).
        let files = vec![make_file("a.md"), make_file("b.md")];
        let mut documents = vec![make_document("a.md"), make_document("b.md")];
        documents[1].block_ids.push("blk1".to_string());
        let mut link = make_wikilink("a.md", "b.md");
        link.kind = LinkKind::Markdown;
        link.block_ref = Some("blk1".to_string());
        documents[0].links.push(link);
        resolve_links(&files, &mut documents);
        let link = &documents[0].links[0];
        assert_eq!(link.status, LinkStatus::Resolved);
        assert_eq!(link.resolved_path, Some("b.md".into()));
    }

    #[test]
    fn markdown_link_block_ref_missing_reports_block_ref_missing() {
        let files = vec![make_file("a.md"), make_file("b.md")];
        let mut documents = vec![make_document("a.md"), make_document("b.md")];
        let mut link = make_wikilink("a.md", "b.md");
        link.kind = LinkKind::Markdown;
        link.block_ref = Some("nope".to_string());
        documents[0].links.push(link);
        resolve_links(&files, &mut documents);
        let link = &documents[0].links[0];
        assert_eq!(link.status, LinkStatus::Unresolved);
        assert_eq!(
            link.unresolved_reason,
            Some(UnresolvedReason::BlockRefMissing)
        );
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
            body_offset: None,
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
    fn alias_only_target_is_dangling_not_resolved() {
        // NRN-455: aliases no longer participate in resolution. A wikilink whose
        // target matches only a doc's `aliases` entry (no path, no stem) is
        // Unresolved/TargetMissing — the repair alias-hint offers the stem rewrite.
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
        assert_eq!(link.status, LinkStatus::Unresolved);
        assert_eq!(
            link.unresolved_reason,
            Some(UnresolvedReason::TargetMissing)
        );
        assert!(link.resolved_path.is_none());
    }

    #[test]
    fn stem_resolves_even_when_another_doc_aliases_it() {
        // Doc-A has stem `foo`; Doc-B aliases `foo`. `[[foo]]` resolves to Doc-A
        // via stem (aliases are irrelevant to resolution).
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
    fn alias_only_target_with_anchor_is_target_missing() {
        // The whole target is unresolvable (alias-only), so the failure is
        // TargetMissing — anchor validation never runs against a non-resolved doc.
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
            body_offset: None,
        });
        let mut link = make_wikilink("src.md", "Vault Memory");
        link.anchor = Some("Architecture".to_string());
        documents[1].links.push(link);
        resolve_links(&files, &mut documents);
        let link = &documents[1].links[0];
        assert_eq!(link.status, LinkStatus::Unresolved);
        assert_eq!(
            link.unresolved_reason,
            Some(UnresolvedReason::TargetMissing)
        );
    }
}
