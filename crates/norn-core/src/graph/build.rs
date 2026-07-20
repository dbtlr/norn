//! Vault walking and graph-index construction — the full walk + parse pipeline.
//!
//! [`build_index_with_options`] walks the vault root under the configured ignore
//! globs and produces a [`GraphIndex`] of [`Document`]s carrying their
//! frontmatter, links, headings, block ids, and aliases, then resolves every
//! link. The per-file text work is delegated: frontmatter extraction, heading and
//! wikilink-token syntax to `norn-frontmatter`; Markdown-link and wikilink
//! link-model extraction and resolution to [`crate::links`].

use std::fs;
use std::path::Path;

use crate::domain::{Diagnostic, Document, GraphIndex, Severity, SourceSpan, VaultFile};
use crate::links::wikilink::parse_frontmatter_wikilinks;
use crate::links::{parse_markdown_links, parse_wikilinks, resolve_links};
use camino::{Utf8Path, Utf8PathBuf};
use norn_frontmatter::frontmatter::extract_frontmatter;
use norn_frontmatter::heading::parse_headings;
use norn_frontmatter::wikilink::parse_block_ids;
use walkdir::WalkDir;

use super::pattern::pattern_matches_path;
use super::{IndexError, IndexOptions};

/// Build a graph index with default options — a `#[cfg(test)]` convenience used
/// by resolution and pipeline tests.
#[cfg(test)]
pub fn build_index(root: impl AsRef<Utf8Path>) -> Result<GraphIndex, IndexError> {
    build_index_with_options(root, &IndexOptions::default())
}

/// Walk `root` and build a fully resolved [`GraphIndex`]. Every graph-visible
/// file becomes a [`VaultFile`]; every Markdown file additionally becomes a parsed
/// [`Document`]. Ignored paths are recorded separately. Links are resolved after
/// the full walk so cross-document references see every document.
pub fn build_index_with_options(
    root: impl AsRef<Utf8Path>,
    options: &IndexOptions,
) -> Result<GraphIndex, IndexError> {
    let root = root.as_ref().to_path_buf();
    if !root.exists() {
        return Err(IndexError::MissingRoot(root));
    }
    if !root.is_dir() {
        return Err(IndexError::RootNotDirectory(root));
    }

    let mut files = Vec::new();
    let mut ignored_files = Vec::new();
    let mut documents = Vec::new();

    visit_graph_files(&root, &root, |path, relative_path| {
        if is_ignored(relative_path, &options.ignore) {
            ignored_files.push(relative_path.to_owned());
            return;
        }
        files.push(parse_file(&root, path));
        if is_markdown(path) {
            documents.push(parse_document(&root, path, options.alias_field.as_deref()));
        }
    })?;

    files.sort_by(|a, b| a.path.cmp(&b.path));
    ignored_files.sort();
    documents.sort_by(|a, b| a.path.cmp(&b.path));
    resolve_links(&files, &mut documents);

    Ok(GraphIndex {
        root,
        files,
        ignored_files,
        documents,
    })
}

/// Return the graph-visible Markdown paths at or below one vault-relative
/// path, using the exact file walk, hidden-component, extension, and ignore
/// semantics used by [`build_index_with_options`]. Publication verification
/// uses this affected-subtree scan to prove that an excluded path did not gain
/// an unparsed document before commit.
pub fn graph_visible_markdown_under(
    root: &Utf8Path,
    subtree: &Utf8Path,
    ignore: &[String],
) -> Result<Vec<Utf8PathBuf>, IndexError> {
    let mut documents = Vec::new();
    if !subtree_is_reachable_by_graph_walk(root, subtree) {
        return Ok(documents);
    }
    let absolute = root.join(subtree);
    visit_graph_files(root, &absolute, |path, relative_path| {
        if is_markdown(path) && !is_ignored(relative_path, ignore) {
            documents.push(relative_path.to_owned());
        }
    })?;
    documents.sort();
    Ok(documents)
}

fn subtree_is_reachable_by_graph_walk(root: &Utf8Path, subtree: &Utf8Path) -> bool {
    // The full graph walk applies its hidden-entry filter to the depth-zero
    // root too (NRN-76). Preserve that existing behavior when a subtree scan
    // starts below the root, rather than accidentally making the verifier see
    // files that the graph itself cannot see.
    if is_hidden(root.as_std_path()) {
        return false;
    }
    let mut absolute = root.to_path_buf();
    let mut components = subtree.components().peekable();
    while let Some(component) = components.next() {
        let name = match component {
            camino::Utf8Component::Normal(name) => name,
            camino::Utf8Component::CurDir => continue,
            camino::Utf8Component::Prefix(_)
            | camino::Utf8Component::RootDir
            | camino::Utf8Component::ParentDir => return false,
        };
        if name.starts_with('.') {
            return false;
        }
        absolute.push(name);
        let Ok(metadata) = std::fs::symlink_metadata(absolute.as_std_path()) else {
            return false;
        };
        if metadata.file_type().is_symlink() {
            return false;
        }
        if components.peek().is_some() && !metadata.file_type().is_dir() {
            return false;
        }
    }
    true
}

fn visit_graph_files(
    root: &Utf8Path,
    scan_root: &Utf8Path,
    mut visit: impl FnMut(&Utf8Path, &Utf8Path),
) -> Result<(), IndexError> {
    for entry in WalkDir::new(scan_root)
        .into_iter()
        .filter_entry(|entry| !is_hidden(entry.path()))
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
    {
        let path = Utf8PathBuf::from_path_buf(entry.path().to_path_buf())
            .map_err(|path| IndexError::NonUtf8Path(path.display().to_string()))?;
        let relative_path = path.strip_prefix(root).unwrap_or(&path);
        visit(&path, relative_path);
    }
    Ok(())
}

fn parse_file(root: &Utf8Path, absolute_path: &Utf8Path) -> VaultFile {
    let path = absolute_path
        .strip_prefix(root)
        .unwrap_or(absolute_path)
        .to_path_buf();
    let stem = path.file_stem().unwrap_or_default().to_string();
    let extension = path.extension().map(ToString::to_string);
    // Only Markdown documents need a content hash (used by repair preconditions).
    // Non-Markdown files are tracked by path identity alone.
    let hash = if is_markdown(absolute_path) {
        fs::read(absolute_path)
            .ok()
            .map(|content| blake3::hash(&content).to_hex().to_string())
    } else {
        None
    };

    VaultFile {
        path,
        stem,
        extension,
        hash,
    }
}

fn parse_document(
    root: &Utf8Path,
    absolute_path: &Utf8Path,
    alias_field: Option<&str>,
) -> Document {
    let path = absolute_path
        .strip_prefix(root)
        .unwrap_or(absolute_path)
        .to_path_buf();
    let stem = path.file_stem().unwrap_or_default().to_string();
    let mut diagnostics = Vec::new();

    let content = match fs::read_to_string(absolute_path) {
        Ok(content) => content,
        Err(error) => {
            return Document {
                path,
                stem,
                hash: String::new(),
                frontmatter: None,
                body_text: String::new(),
                headings: Vec::new(),
                block_ids: Vec::new(),
                links: Vec::new(),
                diagnostics: vec![Diagnostic::error("read-failed", "failed to read document")
                    .with_detail(error.to_string())],
                aliases: vec![],
                alias_malformed: vec![],
            };
        }
    };

    let hash = blake3::hash(content.as_bytes()).to_hex().to_string();
    // NRN-385: flag a leading UTF-8 BOM as a graph diagnostic — a rewrite-only
    // check the donor never had (the oracle silently reads a BOM'd doc as
    // frontmatter-less; NRN-349 only recognizes the block past it, it never
    // reports the byte itself). Detected on the RAW content before
    // `extract_frontmatter` steps past the BOM for recognition, so this fires
    // whether or not the document's frontmatter is recognized — the two are
    // orthogonal facts (a BOM'd doc with no frontmatter never reaches this
    // diagnostic any other way, since `body_text` for that shape is the whole
    // file and would carry the byte, but a BOM'd doc WITH recognized
    // frontmatter has nothing else in `Document` recording the leading byte).
    if content.starts_with('\u{feff}') {
        diagnostics.push(Diagnostic::warning(
            "bom-marker",
            "document begins with a UTF-8 byte-order mark (BOM)",
        ));
    }
    let (frontmatter, frontmatter_range, body, body_start) =
        extract_frontmatter(&content, &mut diagnostics);
    let body_text = body.to_string();
    // `parse_headings` reports spans relative to `body`; re-base them to
    // content-absolute (as the donor's single-pass `commonmark` did) so a
    // document's heading spans stay consistent with its link spans and with the
    // pre-rewrite output contract.
    let headings = parse_headings(body)
        .into_iter()
        .map(|mut heading| {
            heading.source_span = heading
                .source_span
                .map(|span| SourceSpan::at(&content, body_start + span.byte_offset));
            heading
        })
        .collect();
    let mut links = parse_markdown_links(&path, &content, body, body_start);
    links.extend(parse_wikilinks(&path, &content, body, body_start));
    if let Some(frontmatter) = &frontmatter {
        links.extend(parse_frontmatter_wikilinks(
            &path,
            &content,
            frontmatter_range,
            frontmatter,
        ));
    }
    let block_ids = parse_block_ids(body);

    let (aliases, alias_malformed) = if let Some(field) = alias_field {
        super::aliases::parse_aliases(frontmatter.as_ref(), field)
    } else {
        (Vec::new(), Vec::new())
    };

    Document {
        path,
        stem,
        hash,
        frontmatter,
        body_text,
        headings,
        block_ids,
        links,
        diagnostics,
        aliases,
        alias_malformed,
    }
}

/// True when `path` has the Markdown extension recognized by the graph.
///
/// Cache change detection shares this predicate so a case-insensitive graph
/// document cannot be omitted from a subsequent incremental refresh.
pub fn is_markdown(path: &Utf8Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.'))
}

/// True if `path` (vault-relative) matches any `files.ignore` glob. Shared by
/// the cache-build scan gate and the incremental change detector so both agree
/// on exactly which paths are excluded from the graph (NRN-117).
pub fn is_ignored(path: &Utf8Path, patterns: &[String]) -> bool {
    patterns
        .iter()
        .map(|pattern| pattern.trim())
        .filter(|pattern| !pattern.is_empty())
        .any(|pattern| pattern_matches_path(pattern, path))
}

/// A [`Document`]'s diagnostics with their `detail` field stripped — the terse
/// form for surfaces that show only the coded message.
pub fn concise_diagnostics(document: &Document) -> Vec<Diagnostic> {
    document
        .diagnostics
        .iter()
        .map(|diagnostic| Diagnostic {
            severity: diagnostic.severity,
            code: diagnostic.code.clone(),
            message: diagnostic.message.clone(),
            detail: None,
        })
        .collect()
}

/// True when any document in the index carries an error-severity diagnostic.
pub fn has_errors(index: &GraphIndex) -> bool {
    index.documents.iter().any(|document| {
        document
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == Severity::Error)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::LinkStatus;
    use tempfile::TempDir;

    fn vault() -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("vault")).unwrap();
        std::fs::create_dir(&root).unwrap();
        (tmp, root)
    }

    fn write(root: &Utf8Path, rel: &str, content: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent.as_std_path()).unwrap();
        }
        std::fs::write(path.as_std_path(), content).unwrap();
    }

    #[test]
    fn subtree_scan_matches_full_walk_for_hidden_vault_root() {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join(".hidden-vault")).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("visible.md"), "---\n---\nVISIBLE\n").unwrap();

        let full = build_index(&root).unwrap();
        let subtree =
            graph_visible_markdown_under(&root, Utf8Path::new("visible.md"), &[]).unwrap();
        let full_paths: Vec<_> = full
            .documents
            .iter()
            .map(|document| document.path.clone())
            .collect();

        assert!(
            full_paths.is_empty(),
            "the existing full walk excludes a hidden vault root (NRN-76)"
        );
        assert_eq!(subtree, full_paths);
    }

    #[cfg(unix)]
    #[test]
    fn subtree_scan_rejects_symlink_component() {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("vault")).unwrap();
        let real = root.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("note.md"), "---\n---\nNOTE\n").unwrap();
        std::os::unix::fs::symlink(real.as_std_path(), root.join("link").as_std_path()).unwrap();

        let subtree =
            graph_visible_markdown_under(&root, Utf8Path::new("link/note.md"), &[]).unwrap();

        assert!(
            subtree.is_empty(),
            "the graph walk does not follow a symlink component"
        );
    }

    #[test]
    fn subtree_scan_rejects_non_directory_intermediate_component() {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().join("vault")).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("not-a-dir"), "plain file").unwrap();

        let subtree =
            graph_visible_markdown_under(&root, Utf8Path::new("not-a-dir/note.md"), &[]).unwrap();

        assert!(
            subtree.is_empty(),
            "the graph walk cannot traverse through a regular file"
        );
    }

    #[test]
    fn missing_root_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let missing = Utf8PathBuf::from_path_buf(tmp.path().join("nope")).unwrap();
        assert!(matches!(
            build_index(&missing),
            Err(IndexError::MissingRoot(_))
        ));
    }

    #[test]
    fn root_that_is_a_file_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let file = Utf8PathBuf::from_path_buf(tmp.path().join("file")).unwrap();
        std::fs::write(&file, "x").unwrap();
        assert!(matches!(
            build_index(&file),
            Err(IndexError::RootNotDirectory(_))
        ));
    }

    #[test]
    fn indexes_documents_and_resolves_links() {
        let (_tmp, root) = vault();
        write(
            &root,
            "alpha.md",
            "# Alpha\n\nLinks to [[beta]], [[missing]], and [[duplicate]].\n",
        );
        write(&root, "beta.md", "# Beta\n");
        // Two documents share the `duplicate` stem → ambiguous.
        write(&root, "one/duplicate.md", "# Dup one\n");
        write(&root, "two/duplicate.md", "# Dup two\n");

        let index = build_index(&root).unwrap();

        let alpha = index
            .documents
            .iter()
            .find(|document| document.path == "alpha.md")
            .unwrap();
        assert_eq!(alpha.headings[0].text, "Alpha");
        assert!(alpha
            .links
            .iter()
            .any(|link| link.target == "beta" && link.status == LinkStatus::Resolved));
        assert!(alpha
            .links
            .iter()
            .any(|link| link.target == "missing" && link.status == LinkStatus::Unresolved));
        assert!(alpha
            .links
            .iter()
            .any(|link| link.target == "duplicate" && link.status == LinkStatus::Ambiguous));
    }

    #[test]
    fn markdown_links_and_body_wikilinks_appear_in_document_order() {
        // Link array order is external contract: markdown links first, then body
        // wikilinks, then frontmatter wikilinks.
        let (_tmp, root) = vault();
        write(
            &root,
            "a.md",
            "---\nrelated: \"[[fm-link]]\"\n---\n[MD](b.md) then [[wiki]]\n",
        );
        write(&root, "b.md", "# B\n");
        write(&root, "wiki.md", "# Wiki\n");
        write(&root, "fm-link.md", "# FM\n");

        let index = build_index(&root).unwrap();
        let a = index
            .documents
            .iter()
            .find(|document| document.path == "a.md")
            .unwrap();
        let targets: Vec<&str> = a.links.iter().map(|link| link.target.as_str()).collect();
        assert_eq!(targets, vec!["b.md", "wiki", "fm-link"]);
    }

    #[test]
    fn heading_span_is_content_absolute_past_frontmatter() {
        // A document with frontmatter: the heading's recorded byte_offset must be
        // absolute in the file (past the frontmatter block), matching the donor's
        // single-pass span, not body-relative.
        let (_tmp, root) = vault();
        let content = "---\ntitle: A\n---\n# Heading\n";
        write(&root, "a.md", content);
        let index = build_index(&root).unwrap();
        let a = index.documents.iter().find(|d| d.path == "a.md").unwrap();
        let span = a.headings[0].source_span.unwrap();
        let expected = content.find("# Heading").unwrap();
        assert_eq!(span.byte_offset, expected);
        assert!(span.byte_offset > 0, "must be past the frontmatter block");
    }

    #[test]
    fn malformed_frontmatter_is_a_warning() {
        let (_tmp, root) = vault();
        write(&root, "broken.md", "---\ntitle: [broken\n---\n\n# Broken\n");
        let index = build_index(&root).unwrap();
        let broken = index
            .documents
            .iter()
            .find(|document| document.path == "broken.md")
            .unwrap();
        assert_eq!(broken.diagnostics[0].code, "frontmatter-parse-failed");
        assert!(!has_errors(&index));
    }

    #[test]
    fn build_index_populates_aliases_when_configured() {
        let (_tmp, root) = vault();
        write(
            &root,
            "vault-memory.md",
            "---\naliases:\n  - Vault Memory\n  - VM\n---\n# Vault Memory\n",
        );
        write(
            &root,
            "other.md",
            "---\naliases:\n  - 42\n  - nested:\n      bad: value\n---\n# Other\n",
        );

        let options = IndexOptions {
            ignore: vec![],
            alias_field: Some("aliases".into()),
        };
        let index = build_index_with_options(&root, &options).unwrap();

        let vm = index
            .documents
            .iter()
            .find(|d| d.path == "vault-memory.md")
            .unwrap();
        assert_eq!(
            vm.aliases,
            vec!["vault memory".to_string(), "vm".to_string()]
        );
        assert!(vm.alias_malformed.is_empty());

        let other = index
            .documents
            .iter()
            .find(|d| d.path == "other.md")
            .unwrap();
        assert_eq!(other.aliases, vec!["42".to_string()]);
        assert_eq!(other.alias_malformed.len(), 1);
    }

    #[test]
    fn build_index_skips_aliases_when_unconfigured() {
        let (_tmp, root) = vault();
        write(
            &root,
            "vault-memory.md",
            "---\naliases:\n  - Vault Memory\n---\n# Vault Memory\n",
        );
        let index = build_index_with_options(&root, &IndexOptions::default()).unwrap();
        for doc in &index.documents {
            assert!(
                doc.aliases.is_empty(),
                "expected no aliases for {}",
                doc.path
            );
            assert!(doc.alias_malformed.is_empty());
        }
    }

    #[test]
    fn ignored_paths_are_recorded_and_excluded_from_documents() {
        let (_tmp, root) = vault();
        write(&root, "keep.md", "# Keep\n");
        write(&root, "drafts/skip.md", "# Skip\n");

        let options = IndexOptions {
            ignore: vec!["drafts/**".to_string()],
            alias_field: None,
        };
        let index = build_index_with_options(&root, &options).unwrap();

        assert!(index.documents.iter().any(|d| d.path == "keep.md"));
        assert!(!index.documents.iter().any(|d| d.path == "drafts/skip.md"));
        assert!(index.ignored_files.iter().any(|p| p == "drafts/skip.md"));
    }

    #[test]
    fn hidden_files_are_never_walked() {
        let (_tmp, root) = vault();
        write(&root, "visible.md", "# V\n");
        write(&root, ".hidden.md", "# H\n");
        let index = build_index(&root).unwrap();
        assert!(index.documents.iter().any(|d| d.path == "visible.md"));
        assert!(!index.documents.iter().any(|d| d.path == ".hidden.md"));
    }

    #[test]
    fn non_markdown_files_are_tracked_without_a_hash() {
        let (_tmp, root) = vault();
        write(&root, "note.md", "# Note\n");
        write(&root, "image.png", "not really a png");
        let index = build_index(&root).unwrap();

        let md = index.files.iter().find(|f| f.path == "note.md").unwrap();
        assert!(md.hash.is_some());
        let png = index.files.iter().find(|f| f.path == "image.png").unwrap();
        assert!(png.hash.is_none());
        // Only the Markdown file becomes a document.
        assert!(!index.documents.iter().any(|d| d.path == "image.png"));
    }

    #[test]
    fn bom_prefixed_document_gets_bom_marker_diagnostic() {
        // NRN-385: detected on the raw file content, regardless of whether the
        // frontmatter itself is recognized past the BOM (NRN-349).
        let (_tmp, root) = vault();
        write(&root, "bom.md", "\u{feff}---\ntitle: hi\n---\nbody\n");
        let index = build_index(&root).unwrap();
        let doc = index.documents.iter().find(|d| d.path == "bom.md").unwrap();
        assert!(
            doc.diagnostics.iter().any(|d| d.code == "bom-marker"),
            "expected a bom-marker diagnostic, got: {:?}",
            doc.diagnostics
        );
        // Frontmatter is still recognized past the BOM (NRN-349 unaffected).
        assert_eq!(
            doc.frontmatter.as_ref().and_then(|fm| fm.get("title")),
            Some(&serde_json::json!("hi"))
        );
    }

    #[test]
    fn bom_prefixed_document_with_no_frontmatter_still_gets_diagnostic() {
        let (_tmp, root) = vault();
        write(&root, "bom-plain.md", "\u{feff}just a body\n");
        let index = build_index(&root).unwrap();
        let doc = index
            .documents
            .iter()
            .find(|d| d.path == "bom-plain.md")
            .unwrap();
        assert!(doc.diagnostics.iter().any(|d| d.code == "bom-marker"));
    }

    #[test]
    fn document_without_bom_has_no_bom_marker_diagnostic() {
        let (_tmp, root) = vault();
        write(&root, "clean.md", "---\ntitle: hi\n---\nbody\n");
        let index = build_index(&root).unwrap();
        let doc = index
            .documents
            .iter()
            .find(|d| d.path == "clean.md")
            .unwrap();
        assert!(!doc.diagnostics.iter().any(|d| d.code == "bom-marker"));
    }

    #[test]
    fn concise_diagnostics_strips_detail() {
        let diagnostic = Diagnostic::error("read-failed", "failed").with_detail("io error");
        let doc = Document {
            path: "a.md".into(),
            stem: "a".into(),
            hash: String::new(),
            frontmatter: None,
            body_text: String::new(),
            headings: vec![],
            block_ids: vec![],
            links: vec![],
            diagnostics: vec![diagnostic],
            aliases: vec![],
            alias_malformed: vec![],
        };
        let concise = concise_diagnostics(&doc);
        assert_eq!(concise[0].detail, None);
        assert_eq!(concise[0].code, "read-failed");
    }
}
