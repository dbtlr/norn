use std::fs;
use std::path::Path;

use crate::core::{Diagnostic, Document, GraphIndex, Severity, VaultFile};
use crate::frontmatter::extract_frontmatter;
use crate::links::{
    parse_block_ids, parse_commonmark, parse_frontmatter_wikilinks, parse_wikilinks, resolve_links,
};
use camino::{Utf8Path, Utf8PathBuf};
use walkdir::WalkDir;

use super::pattern::pattern_matches_path;
use super::{IndexError, IndexOptions};

#[cfg(test)]
pub fn build_index(root: impl AsRef<Utf8Path>) -> Result<GraphIndex, IndexError> {
    build_index_with_options(root, &IndexOptions::default())
}

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
pub(crate) fn graph_visible_markdown_under(
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
    let (frontmatter, frontmatter_range, body, body_start) =
        extract_frontmatter(&content, &mut diagnostics);
    let body_text = body.to_string();
    let (headings, mut links) = parse_commonmark(&path, &content, body, body_start);
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
pub(crate) fn is_markdown(path: &Utf8Path) -> bool {
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
pub(crate) fn is_ignored(path: &Utf8Path, patterns: &[String]) -> bool {
    patterns
        .iter()
        .map(|pattern| pattern.trim())
        .filter(|pattern| !pattern.is_empty())
        .any(|pattern| pattern_matches_path(pattern, path))
}

pub fn concise_diagnostics(document: &Document) -> Vec<Diagnostic> {
    document
        .diagnostics
        .iter()
        .map(|diagnostic| Diagnostic {
            severity: diagnostic.severity.clone(),
            code: diagnostic.code.clone(),
            message: diagnostic.message.clone(),
            detail: None,
        })
        .collect()
}

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
    use crate::core::LinkStatus;
    use tempfile::TempDir;

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

    #[test]
    fn indexes_documents_and_resolves_links() {
        let index = build_index(Utf8Path::new("fixtures/basic")).unwrap();
        assert_eq!(index.documents.len(), 10);

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
    fn malformed_frontmatter_is_a_warning() {
        let index = build_index(Utf8Path::new("fixtures/basic")).unwrap();
        let broken = index
            .documents
            .iter()
            .find(|document| document.path == "broken-frontmatter.md")
            .unwrap();
        assert_eq!(broken.diagnostics[0].code, "frontmatter-parse-failed");
    }

    #[test]
    fn build_index_populates_aliases_when_configured() {
        let options = IndexOptions {
            ignore: vec![],
            alias_field: Some("aliases".into()),
            ..Default::default()
        };
        let index =
            build_index_with_options(Utf8Path::new("fixtures/alias-basic"), &options).unwrap();

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
        let options = IndexOptions::default();
        let index =
            build_index_with_options(Utf8Path::new("fixtures/alias-basic"), &options).unwrap();
        for doc in &index.documents {
            assert!(
                doc.aliases.is_empty(),
                "expected no aliases for {}",
                doc.path
            );
            assert!(doc.alias_malformed.is_empty());
        }
    }
}
