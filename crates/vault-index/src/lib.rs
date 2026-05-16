use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path};

use camino::{Utf8Path, Utf8PathBuf};
use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};
use regex::Regex;
use vault_core::{
    Diagnostic, Document, GraphIndex, Heading, Link, LinkKind, LinkStatus, Severity, SourceSpan,
};
use walkdir::WalkDir;

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("vault root does not exist: {0}")]
    MissingRoot(Utf8PathBuf),
    #[error("vault root is not a directory: {0}")]
    RootNotDirectory(Utf8PathBuf),
    #[error("path is not valid UTF-8: {0}")]
    NonUtf8Path(String),
}

pub fn build_index(root: impl AsRef<Utf8Path>) -> Result<GraphIndex, IndexError> {
    let root = root.as_ref().to_path_buf();
    if !root.exists() {
        return Err(IndexError::MissingRoot(root));
    }
    if !root.is_dir() {
        return Err(IndexError::RootNotDirectory(root));
    }

    let mut documents = Vec::new();

    for entry in WalkDir::new(&root)
        .into_iter()
        .filter_entry(|entry| !is_hidden(entry.path()))
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
    {
        if !is_markdown(entry.path()) {
            continue;
        }

        let path = Utf8PathBuf::from_path_buf(entry.path().to_path_buf())
            .map_err(|path| IndexError::NonUtf8Path(path.display().to_string()))?;
        documents.push(parse_document(&root, &path));
    }

    documents.sort_by(|a, b| a.path.cmp(&b.path));
    resolve_links(&mut documents);

    Ok(GraphIndex { root, documents })
}

fn parse_document(root: &Utf8Path, absolute_path: &Utf8Path) -> Document {
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
                headings: Vec::new(),
                links: Vec::new(),
                diagnostics: vec![Diagnostic::error("read-failed", "failed to read document")
                    .with_detail(error.to_string())],
            };
        }
    };

    let hash = blake3::hash(content.as_bytes()).to_hex().to_string();
    let (frontmatter, body, body_start) = extract_frontmatter(&content, &mut diagnostics);
    let (headings, mut links) = parse_commonmark(&path, &content, body, body_start);
    links.extend(parse_wikilinks(&path, &content, body, body_start));

    Document {
        path,
        stem,
        hash,
        frontmatter,
        headings,
        links,
        diagnostics,
    }
}

fn extract_frontmatter<'a>(
    content: &'a str,
    diagnostics: &mut Vec<Diagnostic>,
) -> (Option<serde_json::Value>, &'a str, usize) {
    let Some(after_open) = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
    else {
        return (None, content, 0);
    };

    let mut offset = content.len() - after_open.len();
    for line in after_open.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            let yaml = &content[4..offset];
            let body_start = offset + line.len();
            let body = &content[body_start..];
            return match serde_yaml::from_str::<serde_yaml::Value>(yaml) {
                Ok(value) => match serde_json::to_value(value) {
                    Ok(value) => (Some(value), body, body_start),
                    Err(error) => {
                        diagnostics.push(
                            Diagnostic::warning(
                                "frontmatter-json-conversion-failed",
                                "frontmatter could not be converted to JSON",
                            )
                            .with_detail(error.to_string()),
                        );
                        (None, body, body_start)
                    }
                },
                Err(error) => {
                    diagnostics.push(
                        Diagnostic::warning(
                            "frontmatter-parse-failed",
                            "frontmatter could not be parsed",
                        )
                        .with_detail(error.to_string()),
                    );
                    (None, body, body_start)
                }
            };
        }
        offset += line.len();
    }

    diagnostics.push(Diagnostic::warning(
        "frontmatter-unclosed",
        "frontmatter opening delimiter has no closing delimiter",
    ));
    (None, content, 0)
}

fn parse_commonmark(
    source_path: &Utf8Path,
    content: &str,
    body: &str,
    body_start: usize,
) -> (Vec<Heading>, Vec<Link>) {
    let parser = Parser::new(body).into_offset_iter();
    let mut headings = Vec::new();
    let mut links = Vec::new();
    let mut active_heading: Option<(u8, String, usize)> = None;

    for (event, range) in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                active_heading = Some((
                    heading_level(level),
                    String::new(),
                    body_start + range.start,
                ));
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((level, text, start)) = active_heading.take() {
                    let text = text.trim().to_string();
                    headings.push(Heading {
                        level,
                        slug: slugify(&text),
                        text,
                        source_span: Some(source_span(content, start)),
                    });
                }
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some((_, heading_text, _)) = active_heading.as_mut() {
                    heading_text.push_str(&text);
                }
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                let raw = dest_url.to_string();
                if is_local_markdown_target(&raw) {
                    let (target, anchor) = split_anchor(&raw);
                    links.push(Link {
                        source_path: source_path.to_path_buf(),
                        raw,
                        kind: LinkKind::Markdown,
                        target,
                        label: None,
                        anchor,
                        block_ref: None,
                        source_span: Some(source_span(content, body_start + range.start)),
                        resolved_path: None,
                        candidates: Vec::new(),
                        status: LinkStatus::Unresolved,
                    });
                }
            }
            _ => {}
        }
    }

    (headings, links)
}

fn parse_wikilinks(
    source_path: &Utf8Path,
    content: &str,
    body: &str,
    body_start: usize,
) -> Vec<Link> {
    let wikilink_re = Regex::new(r"(!?)\[\[([^\]]+)\]\]").expect("valid wikilink regex");
    wikilink_re
        .captures_iter(body)
        .filter_map(|captures| {
            let raw = captures.get(0)?.as_str().to_string();
            let is_embed = captures.get(1).is_some_and(|m| m.as_str() == "!");
            let inner = captures.get(2)?.as_str();
            let (target_part, label) = inner
                .split_once('|')
                .map_or((inner, None), |(target, label)| {
                    (target, Some(label.trim().to_string()))
                });
            let (target, anchor, block_ref) = split_anchor_or_block_ref(target_part.trim());

            Some(Link {
                source_path: source_path.to_path_buf(),
                raw,
                kind: if is_embed {
                    LinkKind::Embed
                } else {
                    LinkKind::Wikilink
                },
                target,
                label,
                anchor,
                block_ref,
                source_span: Some(source_span(content, body_start + captures.get(0)?.start())),
                resolved_path: None,
                candidates: Vec::new(),
                status: LinkStatus::Unresolved,
            })
        })
        .collect()
}

fn resolve_links(documents: &mut [Document]) {
    let mut by_path: HashMap<String, Utf8PathBuf> = HashMap::new();
    let mut by_stem: HashMap<String, Vec<Utf8PathBuf>> = HashMap::new();

    for document in documents.iter() {
        by_path.insert(document.path.as_str().to_string(), document.path.clone());
        by_stem
            .entry(document.stem.to_lowercase())
            .or_default()
            .push(document.path.clone());
    }

    for document in documents.iter_mut() {
        for link in &mut document.links {
            let candidates = match link.kind {
                LinkKind::Markdown => resolve_markdown_link(&document.path, &link.target, &by_path),
                LinkKind::Wikilink | LinkKind::Embed => {
                    resolve_wikilink(&link.target, &by_path, &by_stem)
                }
            };

            match candidates.as_slice() {
                [single] => {
                    link.status = LinkStatus::Resolved;
                    link.resolved_path = Some(single.clone());
                    link.candidates = Vec::new();
                }
                [] => {
                    link.status = LinkStatus::Unresolved;
                    link.resolved_path = None;
                    link.candidates = Vec::new();
                }
                many => {
                    link.status = LinkStatus::Ambiguous;
                    link.resolved_path = None;
                    link.candidates = many.to_vec();
                }
            }
        }
    }
}

fn resolve_markdown_link(
    source_path: &Utf8Path,
    target: &str,
    by_path: &HashMap<String, Utf8PathBuf>,
) -> Vec<Utf8PathBuf> {
    let base = source_path.parent().unwrap_or_else(|| Utf8Path::new(""));
    let candidate = normalize_relative(base, target);
    by_path
        .get(candidate.as_str())
        .cloned()
        .into_iter()
        .collect()
}

fn resolve_wikilink(
    target: &str,
    by_path: &HashMap<String, Utf8PathBuf>,
    by_stem: &HashMap<String, Vec<Utf8PathBuf>>,
) -> Vec<Utf8PathBuf> {
    if target.contains('/') {
        let direct = if target.ends_with(".md") {
            target.to_string()
        } else {
            format!("{target}.md")
        };
        if let Some(path) = by_path.get(&direct) {
            return vec![path.clone()];
        }
    }

    let stem = Utf8Path::new(target).file_stem().unwrap_or(target);
    by_stem
        .get(&stem.to_lowercase())
        .cloned()
        .unwrap_or_default()
}

fn split_anchor(raw: &str) -> (String, Option<String>) {
    match raw.split_once('#') {
        Some((target, anchor)) => (target.to_string(), Some(anchor.to_string())),
        None => (raw.to_string(), None),
    }
}

fn split_anchor_or_block_ref(raw: &str) -> (String, Option<String>, Option<String>) {
    match raw.split_once('#') {
        Some((target, reference)) if reference.starts_with('^') => {
            (target.to_string(), None, Some(reference[1..].to_string()))
        }
        Some((target, anchor)) => (target.to_string(), Some(anchor.to_string()), None),
        None => (raw.to_string(), None, None),
    }
}

fn source_span(content: &str, byte_offset: usize) -> SourceSpan {
    let prefix = &content[..byte_offset.min(content.len())];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rsplit_once('\n')
        .map_or(prefix.len() + 1, |(_, tail)| tail.len() + 1);

    SourceSpan {
        line,
        column,
        byte_offset,
    }
}

fn is_local_markdown_target(target: &str) -> bool {
    if target.starts_with("http://")
        || target.starts_with("https://")
        || target.starts_with("mailto:")
        || target.starts_with('#')
    {
        return false;
    }

    let (target, _) = split_anchor(target);
    target.ends_with(".md")
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

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn slugify(text: &str) -> String {
    let mut slug = String::new();
    let mut previous_dash = false;

    for ch in text.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            previous_dash = false;
        } else if !previous_dash && !slug.is_empty() {
            slug.push('-');
            previous_dash = true;
        }
    }

    slug.trim_end_matches('-').to_string()
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

    #[test]
    fn indexes_documents_and_resolves_links() {
        let index = build_index(Utf8Path::new("../../fixtures/basic")).unwrap();
        assert_eq!(index.documents.len(), 7);

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
        let index = build_index(Utf8Path::new("../../fixtures/basic")).unwrap();
        let broken = index
            .documents
            .iter()
            .find(|document| document.path == "broken-frontmatter.md")
            .unwrap();
        assert_eq!(broken.diagnostics[0].code, "frontmatter-parse-failed");
    }
}
