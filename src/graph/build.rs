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

    for entry in WalkDir::new(&root)
        .into_iter()
        .filter_entry(|entry| !is_hidden(entry.path()))
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
    {
        let path = Utf8PathBuf::from_path_buf(entry.path().to_path_buf())
            .map_err(|path| IndexError::NonUtf8Path(path.display().to_string()))?;
        let relative_path = path.strip_prefix(&root).unwrap_or(&path).to_path_buf();
        if is_ignored(&relative_path, &options.ignore) {
            ignored_files.push(relative_path);
            continue;
        }
        files.push(parse_file(&root, &path));
        if is_markdown(entry.path()) {
            documents.push(parse_document(&root, &path, options.alias_field.as_deref()));
        }
    }

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

fn parse_file(root: &Utf8Path, absolute_path: &Utf8Path) -> VaultFile {
    let path = absolute_path
        .strip_prefix(root)
        .unwrap_or(absolute_path)
        .to_path_buf();
    let stem = path.file_stem().unwrap_or_default().to_string();
    let extension = path.extension().map(ToString::to_string);
    // Only Markdown documents need a content hash (used by repair preconditions).
    // Non-Markdown files are tracked by path identity alone.
    let hash = if is_markdown(absolute_path.as_std_path()) {
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

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.'))
}

fn is_ignored(path: &Utf8Path, patterns: &[String]) -> bool {
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
