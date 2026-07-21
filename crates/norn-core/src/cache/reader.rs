//! Cache reader: SQLite rows → in-memory `GraphIndex`.

use crate::domain::{
    Diagnostic, Document, GraphIndex, Heading, Link, LinkKind, LinkSourceArea, LinkSourceContext,
    LinkStatus, Severity, SourceSpan, UnresolvedReason, VaultFile,
};
use camino::Utf8PathBuf;

use crate::cache::error::CacheError;

#[cfg(test)]
std::thread_local! {
    /// Deterministic inter-statement publication seam for the snapshot test.
    static AFTER_DOCUMENTS_LOADED: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
fn run_after_documents_loaded_hook() {
    AFTER_DOCUMENTS_LOADED.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(test)]
pub(crate) fn install_after_documents_loaded_hook(hook: impl FnOnce() + 'static) {
    AFTER_DOCUMENTS_LOADED.with(|slot| {
        let previous = slot.borrow_mut().replace(Box::new(hook));
        assert!(previous.is_none(), "graph-load test hook already installed");
    });
}

impl crate::cache::Cache {
    /// Reconstruct a `GraphIndex` from the SQLite tables. Mirrors the shape
    /// `crate::graph::build_index` would produce for the same vault.
    ///
    /// Every relational read is bound to one SQLite snapshot. When called inside
    /// a wider [`Cache::read_snapshot`](crate::cache::Cache::read_snapshot)
    /// phase, it reuses that transaction so consumers can keep follow-up cache
    /// queries on the same generation.
    ///
    /// Diagnostics are not round-tripped from the writer's parse-time warnings in
    /// the same way graph build produces them; `ignored_files` is empty.
    pub fn load_graph_index(&self) -> anyhow::Result<GraphIndex> {
        self.read_snapshot(|cache| {
            let documents = load_documents(&cache.conn, cache.alias_field.as_deref())?;
            #[cfg(test)]
            run_after_documents_loaded_hook();
            let files = load_files(&cache.conn)?;
            Ok(GraphIndex {
                root: cache.vault_root.clone(),
                documents,
                files,
                ignored_files: Vec::new(),
            })
        })
    }
}

fn load_documents(
    conn: &rusqlite::Connection,
    alias_field: Option<&str>,
) -> Result<Vec<Document>, CacheError> {
    let mut docs_stmt = conn.prepare(
        "SELECT path, stem, hash, frontmatter_json, body_text FROM documents ORDER BY path",
    )?;
    let rows = docs_stmt.query_map([], |row| {
        let path: String = row.get(0)?;
        let stem: String = row.get(1)?;
        let hash: String = row.get(2)?;
        let frontmatter_json: Option<String> = row.get(3)?;
        let body_text: String = row.get(4)?;
        Ok((path, stem, hash, frontmatter_json, body_text))
    })?;

    let mut documents = Vec::new();
    for r in rows {
        let (path, stem, hash, fm_json, body_text) = r?;
        let frontmatter = fm_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok());
        let path = Utf8PathBuf::from(path);
        let headings = load_headings(conn, path.as_str())?;
        let block_ids = load_block_ids(conn, path.as_str())?;
        let links = load_links(conn, path.as_str())?;
        let diagnostics = load_diagnostics(conn, path.as_str())?;
        // Re-derive aliases on read: the schema has no dedicated columns for
        // `aliases` / `alias_malformed`, but `frontmatter_json` is the source of
        // truth and `parse_aliases` is cheap. Without this, alias-aware findings
        // silently no-op.
        let (aliases, alias_malformed) = match alias_field {
            Some(field) => crate::graph::parse_aliases(frontmatter.as_ref(), field),
            None => (Vec::new(), Vec::new()),
        };
        documents.push(Document {
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
        });
    }
    Ok(documents)
}

pub(crate) fn load_headings(
    conn: &rusqlite::Connection,
    doc_path: &str,
) -> Result<Vec<Heading>, CacheError> {
    let mut stmt = conn.prepare(
        "SELECT level, text, slug, source_span_line, source_span_column, source_span_byte_offset
         FROM headings WHERE doc_path = ?
         ORDER BY source_span_line, source_span_byte_offset, slug",
    )?;
    let rows = stmt.query_map([doc_path], |r| {
        let level: i64 = r.get(0)?;
        let text: String = r.get(1)?;
        let slug: String = r.get(2)?;
        let line: Option<i64> = r.get(3)?;
        let column: Option<i64> = r.get(4)?;
        let byte_offset: Option<i64> = r.get(5)?;
        let source_span = match (line, column, byte_offset) {
            (Some(l), Some(c), Some(b)) => Some(SourceSpan {
                line: l as usize,
                column: c as usize,
                byte_offset: b as usize,
            }),
            _ => None,
        };
        Ok(Heading {
            level: level as u8,
            text,
            slug,
            source_span,
            // The cache does not persist the heading construct's end offset; a
            // cache-reconstructed heading is display-only (never re-resolved).
            body_offset: None,
        })
    })?;
    let mut headings = Vec::new();
    for r in rows {
        headings.push(r?);
    }
    Ok(headings)
}

pub(crate) fn load_diagnostics(
    conn: &rusqlite::Connection,
    doc_path: &str,
) -> Result<Vec<Diagnostic>, CacheError> {
    let mut stmt = conn.prepare(
        "SELECT severity, code, message, detail FROM diagnostics
         WHERE doc_path = ? ORDER BY rowid",
    )?;
    let rows = stmt.query_map([doc_path], |r| {
        let severity: String = r.get(0)?;
        let code: String = r.get(1)?;
        let message: String = r.get(2)?;
        let detail: Option<String> = r.get(3)?;
        Ok(Diagnostic {
            severity: match severity.as_str() {
                "error" => Severity::Error,
                _ => Severity::Warning,
            },
            code,
            message,
            detail,
        })
    })?;
    let mut diagnostics = Vec::new();
    for r in rows {
        diagnostics.push(r?);
    }
    Ok(diagnostics)
}

pub(crate) fn load_block_ids(
    conn: &rusqlite::Connection,
    doc_path: &str,
) -> Result<Vec<String>, CacheError> {
    let mut stmt =
        conn.prepare("SELECT block_id FROM block_ids WHERE doc_path = ? ORDER BY block_id")?;
    let rows = stmt.query_map([doc_path], |r| r.get::<_, String>(0))?;
    let mut block_ids = Vec::new();
    for r in rows {
        block_ids.push(r?);
    }
    Ok(block_ids)
}

pub(crate) fn load_links(
    conn: &rusqlite::Connection,
    source_path: &str,
) -> Result<Vec<Link>, CacheError> {
    let mut stmt = conn.prepare(
        "SELECT raw, kind, target_raw, resolved_path, anchor, block_ref, label,
                source_span_start, source_span_end, source_span_line, source_span_column,
                source_context, source_context_property, status, unresolved_reason, candidates_json
         FROM links WHERE source_path = ?
         ORDER BY rowid",
    )?;
    let rows = stmt.query_map([source_path], |r| {
        let raw: String = r.get(0)?;
        let kind_str: String = r.get(1)?;
        let target: String = r.get(2)?;
        let resolved: Option<String> = r.get(3)?;
        let anchor: Option<String> = r.get(4)?;
        let block_ref: Option<String> = r.get(5)?;
        let label: Option<String> = r.get(6)?;
        let span_start: Option<i64> = r.get(7)?;
        let _span_end: Option<i64> = r.get(8)?;
        let span_line: Option<i64> = r.get(9)?;
        let span_column: Option<i64> = r.get(10)?;
        let context_str: Option<String> = r.get(11)?;
        let context_property: Option<String> = r.get(12)?;
        let status_str: String = r.get(13)?;
        let unresolved_reason_str: Option<String> = r.get(14)?;
        let candidates_json: Option<String> = r.get(15)?;
        Ok((
            raw,
            kind_str,
            target,
            resolved,
            anchor,
            block_ref,
            label,
            span_start,
            span_line,
            span_column,
            context_str,
            context_property,
            status_str,
            unresolved_reason_str,
            candidates_json,
        ))
    })?;

    let mut links = Vec::new();
    for r in rows {
        let (
            raw,
            kind_str,
            target,
            resolved,
            anchor,
            block_ref,
            label,
            span_start,
            span_line,
            span_column,
            context_str,
            context_property,
            status_str,
            unresolved_reason_str,
            candidates_json,
        ) = r?;
        let kind = match kind_str.as_str() {
            "wikilink" => LinkKind::Wikilink,
            "markdown" => LinkKind::Markdown,
            "embed" => LinkKind::Embed,
            _ => LinkKind::Wikilink,
        };
        let status = match status_str.as_str() {
            "resolved" => LinkStatus::Resolved,
            "ambiguous" => LinkStatus::Ambiguous,
            _ => LinkStatus::Unresolved,
        };
        let source_span = match (span_start, span_line, span_column) {
            (Some(off), Some(l), Some(c)) => Some(SourceSpan {
                line: l as usize,
                column: c as usize,
                byte_offset: off as usize,
            }),
            (Some(off), _, _) => Some(SourceSpan {
                line: 0,
                column: 0,
                byte_offset: off as usize,
            }),
            _ => None,
        };
        let source_context = context_str.as_deref().map(|c| LinkSourceContext {
            area: match c {
                "body" => LinkSourceArea::Body,
                "frontmatter" => LinkSourceArea::Frontmatter,
                _ => LinkSourceArea::Body,
            },
            property: context_property.clone(),
        });
        let unresolved_reason = unresolved_reason_str.as_deref().and_then(|s| match s {
            "target-missing" => Some(UnresolvedReason::TargetMissing),
            "anchor-missing" => Some(UnresolvedReason::AnchorMissing),
            "block-ref-missing" => Some(UnresolvedReason::BlockRefMissing),
            "ambiguous" => Some(UnresolvedReason::Ambiguous),
            _ => None,
        });
        let candidates: Vec<Utf8PathBuf> = candidates_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<Vec<Utf8PathBuf>>(s).ok())
            .unwrap_or_default();
        links.push(Link {
            source_path: Utf8PathBuf::from(source_path),
            raw,
            kind,
            target,
            label,
            anchor,
            block_ref,
            source_span,
            source_context,
            resolved_path: resolved.map(Utf8PathBuf::from),
            unresolved_reason,
            candidates,
            status,
        });
    }
    Ok(links)
}

pub(crate) fn load_files(conn: &rusqlite::Connection) -> Result<Vec<VaultFile>, CacheError> {
    let mut stmt = conn.prepare("SELECT path, ext FROM files ORDER BY path")?;
    let rows = stmt.query_map([], |r| {
        let path: String = r.get(0)?;
        let ext: String = r.get(1)?;
        Ok((path, ext))
    })?;
    let mut files = Vec::new();
    for r in rows {
        let (path_str, ext) = r?;
        let path = Utf8PathBuf::from(path_str);
        let stem = path.file_stem().unwrap_or_default().to_string();
        let extension = if ext.is_empty() { None } else { Some(ext) };
        files.push(VaultFile {
            path,
            stem,
            extension,
            hash: None,
        });
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    fn make_vault() -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(
            root.join("doc.md").as_std_path(),
            "---\ntitle: Doc\n---\n# Heading\n\n[link](other.md)\n",
        )
        .unwrap();
        std::fs::write(
            root.join("other.md").as_std_path(),
            "---\ntitle: Other\n---\n",
        )
        .unwrap();
        (tmp, root)
    }

    #[test]
    fn loaded_index_matches_filesystem_build() {
        let (_tmp, root) = make_vault();
        let direct = crate::graph::build_index_with_options(&root, &Default::default()).unwrap();

        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.full_build(&root).unwrap();
        let loaded = cache.load_graph_index().unwrap();

        assert_eq!(loaded.documents.len(), direct.documents.len());
        assert_eq!(loaded.files.len(), direct.files.len());
        let direct_paths: BTreeSet<_> = direct.documents.iter().map(|d| d.path.clone()).collect();
        let loaded_paths: BTreeSet<_> = loaded.documents.iter().map(|d| d.path.clone()).collect();
        assert_eq!(direct_paths, loaded_paths);
    }

    #[test]
    fn graph_index_load_holds_one_snapshot_across_documents_and_files() {
        let (_tmp, root) = make_vault();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.full_build(&root).unwrap();
        let db_path = cache.db_path().to_owned();

        super::install_after_documents_loaded_hook(move || {
            let writer = rusqlite::Connection::open(db_path.as_std_path()).unwrap();
            writer
                .execute_batch(
                    "BEGIN IMMEDIATE;
                         INSERT INTO documents
                           (path, stem, hash, frontmatter_json, body_text, mtime_ns, size_bytes)
                         VALUES ('delta.md', 'delta', 'delta-hash', NULL, 'Delta', 1, 5);
                         INSERT INTO files (path, ext, size_bytes, mtime_ns)
                         VALUES ('delta.md', 'md', 5, 1);
                         COMMIT;",
                )
                .unwrap();
        });

        let loaded = cache.load_graph_index().unwrap();
        let document_paths: BTreeSet<_> = loaded.documents.iter().map(|doc| &doc.path).collect();
        let file_paths: BTreeSet<_> = loaded.files.iter().map(|file| &file.path).collect();

        assert_eq!(
            document_paths, file_paths,
            "one GraphIndex must not mix pre-publication documents with post-publication files"
        );
    }

    #[test]
    fn loaded_index_preserves_resolved_links() {
        let (_tmp, root) = make_vault();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.full_build(&root).unwrap();
        let loaded = cache.load_graph_index().unwrap();
        let doc = loaded
            .documents
            .iter()
            .find(|d| d.path == "doc.md")
            .unwrap();
        assert_eq!(doc.links.len(), 1);
        assert_eq!(doc.links[0].target, "other.md");
        assert!(doc.links[0].resolved_path.is_some());
    }

    #[test]
    fn loaded_documents_have_aliases_when_cache_was_built_with_alias_field() {
        let tmp = TempDir::new().unwrap();
        let base = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let vault_root = base.join("vault");
        std::fs::create_dir(vault_root.as_std_path()).unwrap();
        std::fs::write(
            vault_root.join("vm.md").as_std_path(),
            "---\naliases:\n  - Vault Memory\n  - VM\n---\n# Vault Memory\n",
        )
        .unwrap();

        let mut cache =
            crate::cache::Cache::open_with_config(&vault_root, Some("aliases")).unwrap();
        cache.full_build(&vault_root).unwrap();

        let cache2 = crate::cache::Cache::open_with_config(&vault_root, Some("aliases")).unwrap();
        let index = cache2.load_graph_index().unwrap();
        let vm = index
            .documents
            .iter()
            .find(|d| d.path == "vm.md")
            .expect("vm.md in loaded docs");
        assert_eq!(
            vm.aliases,
            vec!["vault memory".to_string(), "vm".to_string()],
            "expected lowercased aliases populated on read; got {:?}",
            vm.aliases
        );
    }

    #[test]
    fn loaded_documents_have_empty_aliases_when_cache_built_without_alias_field() {
        let tmp = TempDir::new().unwrap();
        let base = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let vault_root = base.join("vault");
        std::fs::create_dir(vault_root.as_std_path()).unwrap();
        std::fs::write(
            vault_root.join("vm.md").as_std_path(),
            "---\naliases:\n  - Vault Memory\n---\n# Vault Memory\n",
        )
        .unwrap();

        let mut cache = crate::cache::Cache::open_with_config(&vault_root, None).unwrap();
        cache.full_build(&vault_root).unwrap();

        let cache2 = crate::cache::Cache::open_with_config(&vault_root, None).unwrap();
        let index = cache2.load_graph_index().unwrap();
        let vm = index
            .documents
            .iter()
            .find(|d| d.path == "vm.md")
            .expect("vm.md in loaded docs");
        assert!(
            vm.aliases.is_empty(),
            "expected empty aliases when alias_field is None; got {:?}",
            vm.aliases
        );
    }
}
