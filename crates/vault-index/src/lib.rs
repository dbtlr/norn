use std::collections::HashMap;
use std::fs;
use std::ops::Range;
use std::path::{Component, Path};

use camino::{Utf8Path, Utf8PathBuf};
use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};
use regex::Regex;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use vault_core::{
    Diagnostic, Document, GraphIndex, Heading, Link, LinkKind, LinkSourceArea, LinkSourceContext,
    LinkStatus, Severity, SourceSpan, UnresolvedReason, VaultFile,
};
use walkdir::WalkDir;

const CACHE_SCHEMA_VERSION: &str = "2";

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("vault root does not exist: {0}")]
    MissingRoot(Utf8PathBuf),
    #[error("vault root is not a directory: {0}")]
    RootNotDirectory(Utf8PathBuf),
    #[error("path is not valid UTF-8: {0}")]
    NonUtf8Path(String),
    #[error("failed to write SQLite cache: {0}")]
    CacheWriteFailed(#[from] rusqlite::Error),
    #[error("failed to create cache directory {path}: {source}")]
    CacheDirectoryCreateFailed {
        path: Utf8PathBuf,
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheSummary {
    pub cache_path: Utf8PathBuf,
    pub files: usize,
    pub ignored_files: usize,
    pub documents: usize,
    pub links: usize,
    pub diagnostics: usize,
}

#[derive(Debug, Clone, Default)]
pub struct IndexOptions {
    pub ignore: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct VaultConfig {
    #[serde(default)]
    pub graph: GraphConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GraphConfig {
    #[serde(default)]
    pub ignore: Vec<String>,
}

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
            documents.push(parse_document(&root, &path));
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

pub fn write_sqlite_cache(
    index: &GraphIndex,
    cache: impl AsRef<Utf8Path>,
) -> Result<CacheSummary, IndexError> {
    let cache_path = cache_file_path(cache.as_ref());
    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent).map_err(|source| IndexError::CacheDirectoryCreateFailed {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let mut connection = Connection::open(cache_path.as_str())?;
    initialize_cache_schema(&connection)?;
    let transaction = connection.transaction()?;
    clear_cache(&transaction)?;
    insert_index(&transaction, index)?;
    transaction.commit()?;

    Ok(CacheSummary {
        cache_path,
        files: index.files.len(),
        ignored_files: index.ignored_files.len(),
        documents: index.documents.len(),
        links: index
            .documents
            .iter()
            .map(|document| document.links.len())
            .sum(),
        diagnostics: index
            .documents
            .iter()
            .map(|document| document.diagnostics.len())
            .sum(),
    })
}

fn parse_file(root: &Utf8Path, absolute_path: &Utf8Path) -> VaultFile {
    let path = absolute_path
        .strip_prefix(root)
        .unwrap_or(absolute_path)
        .to_path_buf();
    let stem = path.file_stem().unwrap_or_default().to_string();
    let extension = path.extension().map(ToString::to_string);
    let hash = fs::read(absolute_path)
        .map(|content| blake3::hash(&content).to_hex().to_string())
        .unwrap_or_default();

    VaultFile {
        path,
        stem,
        extension,
        hash,
    }
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
                block_ids: Vec::new(),
                links: Vec::new(),
                diagnostics: vec![Diagnostic::error("read-failed", "failed to read document")
                    .with_detail(error.to_string())],
            };
        }
    };

    let hash = blake3::hash(content.as_bytes()).to_hex().to_string();
    let (frontmatter, frontmatter_range, body, body_start) =
        extract_frontmatter(&content, &mut diagnostics);
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

    Document {
        path,
        stem,
        hash,
        frontmatter,
        headings,
        block_ids,
        links,
        diagnostics,
    }
}

fn extract_frontmatter<'a>(
    content: &'a str,
    diagnostics: &mut Vec<Diagnostic>,
) -> (
    Option<serde_json::Value>,
    Option<Range<usize>>,
    &'a str,
    usize,
) {
    let Some(after_open) = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
    else {
        return (None, None, content, 0);
    };

    let mut offset = content.len() - after_open.len();
    let yaml_start = offset;
    for line in after_open.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            let yaml = &content[yaml_start..offset];
            let yaml_range = yaml_start..offset;
            let body_start = offset + line.len();
            let body = &content[body_start..];
            return match serde_yaml::from_str::<serde_yaml::Value>(yaml) {
                Ok(value) => match serde_json::to_value(value) {
                    Ok(value) => (Some(value), Some(yaml_range), body, body_start),
                    Err(error) => {
                        diagnostics.push(
                            Diagnostic::warning(
                                "frontmatter-json-conversion-failed",
                                "frontmatter could not be converted to JSON",
                            )
                            .with_detail(error.to_string()),
                        );
                        (None, Some(yaml_range), body, body_start)
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
                    (None, Some(yaml_range), body, body_start)
                }
            };
        }
        offset += line.len();
    }

    diagnostics.push(Diagnostic::warning(
        "frontmatter-unclosed",
        "frontmatter opening delimiter has no closing delimiter",
    ));
    (None, None, content, 0)
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
                    let (target, anchor) = split_anchor(&decode_percent_escapes(&raw));
                    links.push(Link {
                        source_path: source_path.to_path_buf(),
                        raw,
                        kind: LinkKind::Markdown,
                        target,
                        label: None,
                        anchor,
                        block_ref: None,
                        source_span: Some(source_span(content, body_start + range.start)),
                        source_context: Some(LinkSourceContext {
                            area: LinkSourceArea::Body,
                            property: None,
                        }),
                        resolved_path: None,
                        unresolved_reason: None,
                        candidates: Vec::new(),
                        status: LinkStatus::Unresolved,
                    });
                }
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                let raw = dest_url.to_string();
                if is_local_file_target(&raw) {
                    let (target, anchor) = split_anchor(&decode_percent_escapes(&raw));
                    links.push(Link {
                        source_path: source_path.to_path_buf(),
                        raw,
                        kind: LinkKind::Embed,
                        target,
                        label: None,
                        anchor,
                        block_ref: None,
                        source_span: Some(source_span(content, body_start + range.start)),
                        source_context: Some(LinkSourceContext {
                            area: LinkSourceArea::Body,
                            property: None,
                        }),
                        resolved_path: None,
                        unresolved_reason: None,
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
    let ignored_ranges = ignored_wikilink_ranges(body);
    parse_wikilinks_in_text(
        source_path,
        body,
        Some((content, body_start, ignored_ranges)),
        Some(LinkSourceContext {
            area: LinkSourceArea::Body,
            property: None,
        }),
    )
}

fn parse_wikilinks_in_text(
    source_path: &Utf8Path,
    text: &str,
    source: Option<(&str, usize, Vec<Range<usize>>)>,
    source_context: Option<LinkSourceContext>,
) -> Vec<Link> {
    let wikilink_re = Regex::new(r"(!?)\[\[([^\]]+)\]\]").expect("valid wikilink regex");
    wikilink_re
        .captures_iter(text)
        .filter_map(|captures| {
            let full_match = captures.get(0)?;
            if source.as_ref().is_some_and(|(_, _, ignored_ranges)| {
                ignored_ranges
                    .iter()
                    .any(|range| ranges_overlap(range, &(full_match.start()..full_match.end())))
            }) {
                return None;
            }

            let source_span = source.as_ref().map(|(content, base_offset, _)| {
                source_span(content, base_offset + full_match.start())
            });

            let raw = full_match.as_str().to_string();
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
                source_span,
                source_context: source_context.clone(),
                resolved_path: None,
                unresolved_reason: None,
                candidates: Vec::new(),
                status: LinkStatus::Unresolved,
            })
        })
        .collect()
}

fn parse_frontmatter_wikilinks(
    source_path: &Utf8Path,
    content: &str,
    frontmatter_range: Option<Range<usize>>,
    frontmatter: &serde_json::Value,
) -> Vec<Link> {
    let Some(object) = frontmatter.as_object() else {
        return Vec::new();
    };

    frontmatter_property_strings(object, content, frontmatter_range)
        .into_iter()
        .flat_map(|property_string| {
            parse_wikilinks_in_text(
                source_path,
                property_string.text,
                property_string
                    .offset
                    .map(|offset| (content, offset, Vec::new())),
                Some(LinkSourceContext {
                    area: LinkSourceArea::Frontmatter,
                    property: Some(property_string.property),
                }),
            )
        })
        .collect()
}

struct FrontmatterPropertyString<'a> {
    property: String,
    text: &'a str,
    offset: Option<usize>,
}

fn frontmatter_property_strings<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    content: &str,
    frontmatter_range: Option<Range<usize>>,
) -> Vec<FrontmatterPropertyString<'a>> {
    let mut strings = Vec::new();

    for (property, value) in object {
        match value {
            serde_json::Value::String(text) => strings.push(FrontmatterPropertyString {
                property: property.to_string(),
                text,
                offset: frontmatter_scalar_offset(
                    content,
                    frontmatter_range.clone(),
                    property,
                    text,
                ),
            }),
            serde_json::Value::Array(values) => {
                for text in values.iter().filter_map(|value| value.as_str()) {
                    strings.push(FrontmatterPropertyString {
                        property: property.to_string(),
                        text,
                        offset: frontmatter_list_item_offset(
                            content,
                            frontmatter_range.clone(),
                            property,
                            text,
                        ),
                    });
                }
            }
            _ => {}
        }
    }

    strings
}

fn frontmatter_scalar_offset(
    content: &str,
    frontmatter_range: Option<Range<usize>>,
    property: &str,
    text: &str,
) -> Option<usize> {
    let range = frontmatter_range?;
    let yaml = &content[range.clone()];
    let property_prefix = format!("{property}:");
    let mut line_start = range.start;

    for line in yaml.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if !line.starts_with([' ', '\t']) && trimmed.starts_with(&property_prefix) {
            return line.find(text).map(|offset| line_start + offset);
        }
        line_start += line.len();
    }

    None
}

fn frontmatter_list_item_offset(
    content: &str,
    frontmatter_range: Option<Range<usize>>,
    property: &str,
    text: &str,
) -> Option<usize> {
    let range = frontmatter_range?;
    let yaml = &content[range.clone()];
    let property_prefix = format!("{property}:");
    let mut in_property = false;
    let mut line_start = range.start;

    for line in yaml.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if !line.starts_with([' ', '\t']) {
            in_property = trimmed.starts_with(&property_prefix);
            line_start += line.len();
            continue;
        }

        if in_property && trimmed.starts_with('-') {
            if let Some(offset) = line.find(text) {
                return Some(line_start + offset);
            }
        }

        line_start += line.len();
    }

    None
}

fn ignored_wikilink_ranges(body: &str) -> Vec<Range<usize>> {
    let parser = Parser::new(body).into_offset_iter();
    let mut ignored_ranges = Vec::new();
    let mut active_code_block_start = None;

    for (event, range) in parser {
        match event {
            Event::Code(_) => ignored_ranges.push(range),
            Event::Start(Tag::CodeBlock(_)) => active_code_block_start = Some(range.start),
            Event::End(TagEnd::CodeBlock) => {
                if let Some(start) = active_code_block_start.take() {
                    ignored_ranges.push(start..range.end);
                }
            }
            _ => {}
        }
    }

    ignored_ranges
}

fn ranges_overlap(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}

fn parse_block_ids(body: &str) -> Vec<String> {
    let block_re = Regex::new(r"(?:^|\s)\^([A-Za-z0-9_-]+)\s*$").expect("valid block id regex");
    body.lines()
        .filter_map(|line| {
            block_re
                .captures(line)
                .and_then(|captures| captures.get(1))
                .map(|block_id| block_id.as_str().to_string())
        })
        .collect()
}

fn resolve_links(files: &[VaultFile], documents: &mut [Document]) {
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
    if !is_local_file_target(target) {
        return false;
    }

    let (target, _) = split_anchor(target);
    !target.is_empty()
}

fn is_local_file_target(target: &str) -> bool {
    if target.starts_with("http://")
        || target.starts_with("https://")
        || target.starts_with("mailto:")
        || target.starts_with('#')
    {
        return false;
    }

    true
}

fn decode_percent_escapes(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                output.push((high << 4) | low);
                index += 3;
                continue;
            }
        }

        output.push(bytes[index]);
        index += 1;
    }

    String::from_utf8_lossy(&output).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
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

fn pattern_matches_path(pattern: &str, path: &Utf8Path) -> bool {
    let path = path.as_str();
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return path == prefix || path.starts_with(&format!("{prefix}/"));
    }

    if let Some(extension) = pattern.strip_prefix("*.") {
        return path
            .rsplit_once('.')
            .is_some_and(|(_, path_extension)| path_extension == extension);
    }

    if pattern.contains('*') {
        return glob_star_matches(pattern, path);
    }

    path == pattern
}

fn glob_star_matches(pattern: &str, path: &str) -> bool {
    let parts = pattern.split('*').collect::<Vec<_>>();
    let mut remainder = path;

    if let Some(first) = parts.first() {
        if !first.is_empty() {
            let Some(after_prefix) = remainder.strip_prefix(first) else {
                return false;
            };
            remainder = after_prefix;
        }
    }

    for part in parts.iter().skip(1).take(parts.len().saturating_sub(2)) {
        if part.is_empty() {
            continue;
        }
        let Some(index) = remainder.find(part) else {
            return false;
        };
        remainder = &remainder[index + part.len()..];
    }

    if let Some(last) = parts.last() {
        if !last.is_empty() {
            return remainder.ends_with(last);
        }
    }

    true
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

fn cache_file_path(cache: &Utf8Path) -> Utf8PathBuf {
    if cache.extension().is_some() {
        cache.to_path_buf()
    } else {
        cache.join("graph.sqlite")
    }
}

fn initialize_cache_schema(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        r#"
        DROP TABLE IF EXISTS diagnostics;
        DROP TABLE IF EXISTS metadata;
        DROP TABLE IF EXISTS links;
        DROP TABLE IF EXISTS block_ids;
        DROP TABLE IF EXISTS headings;
        DROP TABLE IF EXISTS documents;
        DROP TABLE IF EXISTS files;

        CREATE TABLE IF NOT EXISTS files (
            path TEXT PRIMARY KEY NOT NULL,
            hash TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS documents (
            path TEXT PRIMARY KEY NOT NULL,
            stem TEXT NOT NULL,
            frontmatter_json TEXT
        );

        CREATE TABLE IF NOT EXISTS headings (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            path TEXT NOT NULL,
            level INTEGER NOT NULL,
            text TEXT NOT NULL,
            slug TEXT NOT NULL,
            line INTEGER,
            column INTEGER,
            byte_offset INTEGER
        );

        CREATE TABLE IF NOT EXISTS block_ids (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            path TEXT NOT NULL,
            block_id TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS links (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            source_path TEXT NOT NULL,
            raw TEXT NOT NULL,
            kind TEXT NOT NULL,
            target TEXT NOT NULL,
            label TEXT,
            anchor TEXT,
            block_ref TEXT,
            status TEXT NOT NULL,
            resolved_path TEXT,
            unresolved_reason TEXT,
            candidates_json TEXT,
            line INTEGER,
            column INTEGER,
            byte_offset INTEGER,
            source_area TEXT,
            source_property TEXT
        );

        CREATE TABLE IF NOT EXISTS diagnostics (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            path TEXT NOT NULL,
            severity TEXT NOT NULL,
            code TEXT NOT NULL,
            message TEXT NOT NULL,
            detail TEXT
        );

        CREATE TABLE IF NOT EXISTS metadata (
            key TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL
        );
        "#,
    )
}

fn clear_cache(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        r#"
        DELETE FROM diagnostics;
        DELETE FROM metadata;
        DELETE FROM links;
        DELETE FROM block_ids;
        DELETE FROM headings;
        DELETE FROM documents;
        DELETE FROM files;
        "#,
    )
}

fn insert_index(connection: &Connection, index: &GraphIndex) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO metadata (key, value) VALUES ('schema_version', ?1)",
        params![CACHE_SCHEMA_VERSION],
    )?;

    for file in &index.files {
        connection.execute(
            "INSERT INTO files (path, hash) VALUES (?1, ?2)",
            params![file.path.as_str(), file.hash],
        )?;
    }

    for document in &index.documents {
        connection.execute(
            "INSERT INTO documents (path, stem, frontmatter_json) VALUES (?1, ?2, ?3)",
            params![
                document.path.as_str(),
                document.stem,
                document
                    .frontmatter
                    .as_ref()
                    .map(|frontmatter| frontmatter.to_string())
            ],
        )?;

        for heading in &document.headings {
            connection.execute(
                "INSERT INTO headings (path, level, text, slug, line, column, byte_offset)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    document.path.as_str(),
                    heading.level,
                    heading.text,
                    heading.slug,
                    heading.source_span.as_ref().map(|span| span.line),
                    heading.source_span.as_ref().map(|span| span.column),
                    heading.source_span.as_ref().map(|span| span.byte_offset),
                ],
            )?;
        }

        for block_id in &document.block_ids {
            connection.execute(
                "INSERT INTO block_ids (path, block_id) VALUES (?1, ?2)",
                params![document.path.as_str(), block_id],
            )?;
        }

        for link in &document.links {
            connection.execute(
                "INSERT INTO links (
                    source_path, raw, kind, target, label, anchor, block_ref, status,
                    resolved_path, unresolved_reason, candidates_json, line, column, byte_offset,
                    source_area, source_property
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                params![
                    link.source_path.as_str(),
                    link.raw,
                    link_kind_name(&link.kind),
                    link.target,
                    link.label,
                    link.anchor,
                    link.block_ref,
                    link_status_name(&link.status),
                    link.resolved_path.as_ref().map(|path| path.as_str()),
                    link.unresolved_reason.as_ref().map(unresolved_reason_name),
                    serde_json::to_string(&link.candidates).unwrap_or_else(|_| "[]".to_string()),
                    link.source_span.as_ref().map(|span| span.line),
                    link.source_span.as_ref().map(|span| span.column),
                    link.source_span.as_ref().map(|span| span.byte_offset),
                    link.source_context
                        .as_ref()
                        .map(|context| link_source_area_name(&context.area)),
                    link.source_context
                        .as_ref()
                        .and_then(|context| context.property.as_deref()),
                ],
            )?;
        }

        for diagnostic in &document.diagnostics {
            connection.execute(
                "INSERT INTO diagnostics (path, severity, code, message, detail)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    document.path.as_str(),
                    severity_name(&diagnostic.severity),
                    diagnostic.code,
                    diagnostic.message,
                    diagnostic.detail,
                ],
            )?;
        }
    }

    Ok(())
}

fn link_kind_name(kind: &LinkKind) -> &'static str {
    match kind {
        LinkKind::Markdown => "markdown",
        LinkKind::Wikilink => "wikilink",
        LinkKind::Embed => "embed",
    }
}

fn link_status_name(status: &LinkStatus) -> &'static str {
    match status {
        LinkStatus::Resolved => "resolved",
        LinkStatus::Unresolved => "unresolved",
        LinkStatus::Ambiguous => "ambiguous",
    }
}

fn link_source_area_name(area: &LinkSourceArea) -> &'static str {
    match area {
        LinkSourceArea::Body => "body",
        LinkSourceArea::Frontmatter => "frontmatter",
    }
}

fn unresolved_reason_name(reason: &UnresolvedReason) -> &'static str {
    match reason {
        UnresolvedReason::TargetMissing => "target-missing",
        UnresolvedReason::AnchorMissing => "anchor-missing",
        UnresolvedReason::BlockRefMissing => "block-ref-missing",
        UnresolvedReason::Ambiguous => "ambiguous",
    }
}

fn severity_name(severity: &Severity) -> &'static str {
    match severity {
        Severity::Warning => "warning",
        Severity::Error => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_documents_and_resolves_links() {
        let index = build_index(Utf8Path::new("../../fixtures/basic")).unwrap();
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
        let index = build_index(Utf8Path::new("../../fixtures/basic")).unwrap();
        let broken = index
            .documents
            .iter()
            .find(|document| document.path == "broken-frontmatter.md")
            .unwrap();
        assert_eq!(broken.diagnostics[0].code, "frontmatter-parse-failed");
    }
}
