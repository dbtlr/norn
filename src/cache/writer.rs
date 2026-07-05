//! Cache writer: full rebuild and (later) incremental update.

use crate::core::{Document, GraphIndex, Link, VaultFile};
use camino::Utf8Path;
use rusqlite::{params, Transaction};

use crate::cache::change_detection::{detect, ChangeDetectOptions, FileChange};
use crate::cache::error::CacheError;

// Superseded by `ChangeDetectOptions` (the live force-hash knob). Kept to
// preserve the writer's option-struct shape; safe to delete in a cleanup pass.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct IndexOptions {
    pub force_hash: bool,
}

#[derive(Debug, Clone, Default)]
pub struct IndexReport {
    pub doc_count: usize,
    pub link_count: usize,
    pub file_count: usize,
    pub duration_ms: u128,
}

impl crate::cache::Cache {
    /// Returns true if a full rebuild has ever stamped this cache (a
    /// `last_full_rebuild_ts` meta row exists). Fresh caches and caches that
    /// have only seen schema/meta init return false.
    fn has_been_built(&self) -> Result<bool, CacheError> {
        let row: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'last_full_rebuild_ts'",
                [],
                |r| r.get(0),
            )
            .ok();
        Ok(row.is_some())
    }

    /// Full rebuild: walk the vault, parse every document, replace all rows.
    /// Used by `norn cache rebuild` and the implicit rebuild after a self-heal trigger.
    pub fn rebuild(&mut self, vault_root: &Utf8Path) -> Result<IndexReport, CacheError> {
        let _lock = crate::cache::lock::WriteLock::acquire(
            &self.cache_dir,
            std::time::Duration::from_secs(5),
        )?;
        let start = std::time::Instant::now();
        let options = crate::graph::IndexOptions {
            ignore: self.files_ignore.clone(),
            alias_field: self.alias_field.clone(),
            ..Default::default()
        };
        let index = crate::graph::build_index_with_options(vault_root, &options)?;

        let tx = self.conn.transaction()?;
        clear_all_rows(&tx)?;
        let mut report = IndexReport::default();
        for doc in &index.documents {
            insert_document(&tx, vault_root, doc, &mut report, &self.index_set)?;
        }
        for file in &index.files {
            insert_file(&tx, vault_root, file)?;
            report.file_count += 1;
        }
        update_meta_rebuild_ts(&tx)?;
        update_meta_alias_field(&tx, self.alias_field.as_deref())?;
        // Only an authoritative open (`Cache::open_with_index`) knows the
        // operator's real declared index set — stamping the hash from a
        // non-authoritative open's unconfigured-default empty set would
        // make a later authoritative open think it's already reconciled.
        // Leave whatever stamp already exists; that later open reconciles.
        if self.index_authoritative {
            update_meta_index_set_hash(&tx, &self.index_set_hash)?;
        }
        tx.commit()?;
        // Refresh query-planner statistics for `document_fields` after a full
        // rewrite; deliberately not run on the per-doc incremental path
        // below, where the row-count delta is too small to matter. Scoped to
        // this one table (rather than a schema-wide `ANALYZE`) so it doesn't
        // perturb existing planner decisions for `links`/`documents` on
        // small test fixtures — see the EXPLAIN-guard tests in query.rs /
        // query_show.rs.
        self.conn.execute("ANALYZE document_fields", [])?;

        report.duration_ms = start.elapsed().as_millis();
        Ok(report)
    }

    /// Incremental update: detect changes against the cached state, then
    /// drop+reinsert only the affected documents. Re-runs the full
    /// `crate::graph::build_index` for parse authority but updates only the
    /// changed-document subset of rows.
    ///
    /// When the cache has never been fully built (no `last_full_rebuild_ts`
    /// meta row), this defers to `rebuild` so attachments and other non-Markdown
    /// files are populated — the cheap change-detector only walks `.md` files.
    pub fn index_incremental(
        &mut self,
        vault_root: &Utf8Path,
        options: &ChangeDetectOptions,
    ) -> Result<IndexReport, CacheError> {
        if !self.has_been_built()? {
            return self.rebuild(vault_root);
        }
        let _lock = crate::cache::lock::WriteLock::acquire(
            &self.cache_dir,
            std::time::Duration::from_secs(5),
        )?;
        let start = std::time::Instant::now();
        let changes = detect(vault_root, self, options)?;
        if changes.is_empty() {
            return Ok(IndexReport::default());
        }

        // Re-parse the affected docs from the filesystem. Aggressive
        // invalidation: re-run build_index on the whole vault and pick out
        // the affected documents. Simpler than scoped parsing, and the
        // per-doc cost dominates only on truly huge vaults where
        // parse-everything beats incremental in total time anyway.
        let options = crate::graph::IndexOptions {
            ignore: self.files_ignore.clone(),
            alias_field: self.alias_field.clone(),
            ..Default::default()
        };
        let fresh_index = crate::graph::build_index_with_options(vault_root, &options)?;
        let fresh_docs: std::collections::HashMap<_, _> = fresh_index
            .documents
            .iter()
            .map(|d| (d.path.clone(), d))
            .collect();

        let tx = self.conn.transaction()?;
        let mut report = IndexReport::default();

        for change in &changes {
            match change {
                FileChange::Deleted(path) => {
                    crate::cache::invalidation::drop_document(&tx, path)?;
                }
                FileChange::Added(path) | FileChange::Modified(path) => {
                    crate::cache::invalidation::drop_document(&tx, path)?;
                    if let Some(doc) = fresh_docs.get(path) {
                        insert_document(&tx, vault_root, doc, &mut report, &self.index_set)?;
                    }
                }
            }
        }

        // Rewrite the entire links table from the fresh index. Link resolution is
        // global, so this is the step that keeps an incremental refresh identical
        // to a full rebuild — the per-doc invalidation above updates doc rows;
        // link resolution is not decomposable per-doc (NRN-126). This supersedes
        // any incoming-link fixup, so no `unresolve_incoming` is needed above.
        rerun_link_resolution(&tx, &fresh_index)?;

        tx.commit()?;

        report.duration_ms = start.elapsed().as_millis();
        Ok(report)
    }
}

/// Rewrite the entire links table from the authoritative fresh index.
///
/// Link resolution is a GLOBAL function of the whole document set: any doc's
/// path, stem, or alias change can re-resolve links in OTHER, unchanged docs —
/// e.g. adding an alias `foo` to one doc resolves a previously-missing
/// `[[foo]]` in another. A per-doc "does this doc link a changed target"
/// heuristic cannot capture alias-driven (or ambiguity-driven) re-resolution,
/// which made an incremental refresh's link findings diverge from a full
/// rebuild's (NRN-126) — a violation of the same-input-same-output principle.
///
/// `fresh_index` already holds the fully resolved links for every doc (it is a
/// whole-vault parse + resolve), so rewriting the table from it makes an
/// incremental refresh identical to a rebuild by construction. The parse is
/// already paid for above; only the links table is fully rewritten (doc/field/
/// heading rows for unchanged docs are left untouched).
fn rerun_link_resolution(tx: &Transaction, fresh_index: &GraphIndex) -> Result<(), CacheError> {
    tx.execute("DELETE FROM links", [])?;
    for doc in &fresh_index.documents {
        for link in &doc.links {
            insert_link(tx, link)?;
        }
    }
    Ok(())
}

fn clear_all_rows(tx: &rusqlite::Transaction) -> Result<(), CacheError> {
    tx.execute("DELETE FROM documents", [])?;
    tx.execute("DELETE FROM document_fields", [])?;
    tx.execute("DELETE FROM files", [])?;
    tx.execute("DELETE FROM links", [])?;
    tx.execute("DELETE FROM headings", [])?;
    tx.execute("DELETE FROM block_ids", [])?;
    tx.execute("DELETE FROM diagnostics", [])?;
    Ok(())
}

fn insert_document(
    tx: &rusqlite::Transaction,
    vault_root: &Utf8Path,
    doc: &Document,
    report: &mut IndexReport,
    index_set: &std::collections::BTreeSet<String>,
) -> Result<(), CacheError> {
    let frontmatter_json = doc
        .frontmatter
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default());
    let absolute = vault_root.join(&doc.path);
    let mtime_ns = mtime_ns(&absolute).unwrap_or(0);
    let size_bytes = size_bytes(&absolute).unwrap_or(0);

    tx.execute(
        "INSERT INTO documents
           (path, stem, hash, frontmatter_json, body_text, mtime_ns, size_bytes)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        params![
            doc.path.as_str(),
            doc.stem,
            doc.hash,
            frontmatter_json,
            doc.body_text,
            mtime_ns,
            size_bytes,
        ],
    )?;
    report.doc_count += 1;

    crate::cache::document_fields::insert_rows(
        tx,
        doc.path.as_str(),
        doc.frontmatter.as_ref(),
        index_set,
    )?;

    for heading in &doc.headings {
        let (line, column, byte_offset): (Option<i64>, Option<i64>, Option<i64>) =
            match &heading.source_span {
                Some(s) => (
                    Some(s.line as i64),
                    Some(s.column as i64),
                    Some(s.byte_offset as i64),
                ),
                None => (None, None, None),
            };
        tx.execute(
            "INSERT OR IGNORE INTO headings
               (doc_path, level, text, slug,
                source_span_line, source_span_column, source_span_byte_offset)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![
                doc.path.as_str(),
                heading.level as i64,
                heading.text,
                heading.slug,
                line,
                column,
                byte_offset,
            ],
        )?;
    }
    for block_id in &doc.block_ids {
        tx.execute(
            "INSERT OR IGNORE INTO block_ids (doc_path, block_id) VALUES (?, ?)",
            params![doc.path.as_str(), block_id],
        )?;
    }
    for link in &doc.links {
        insert_link(tx, link)?;
        report.link_count += 1;
    }
    for diagnostic in &doc.diagnostics {
        insert_diagnostic(tx, doc.path.as_str(), diagnostic)?;
    }

    Ok(())
}

fn insert_diagnostic(
    tx: &rusqlite::Transaction,
    doc_path: &str,
    diagnostic: &crate::core::Diagnostic,
) -> Result<(), CacheError> {
    let severity = match diagnostic.severity {
        crate::core::Severity::Warning => "warning",
        crate::core::Severity::Error => "error",
    };
    tx.execute(
        "INSERT INTO diagnostics (doc_path, severity, code, message, detail)
         VALUES (?, ?, ?, ?, ?)",
        params![
            doc_path,
            severity,
            diagnostic.code,
            diagnostic.message,
            diagnostic.detail,
        ],
    )?;
    Ok(())
}

fn link_kind_str(kind: &crate::core::LinkKind) -> &'static str {
    match kind {
        crate::core::LinkKind::Wikilink => "wikilink",
        crate::core::LinkKind::Markdown => "markdown",
        crate::core::LinkKind::Embed => "embed",
    }
}

fn link_status_str(status: &crate::core::LinkStatus) -> &'static str {
    match status {
        crate::core::LinkStatus::Resolved => "resolved",
        crate::core::LinkStatus::Unresolved => "unresolved",
        crate::core::LinkStatus::Ambiguous => "ambiguous",
    }
}

fn link_source_area_str(area: &crate::core::LinkSourceArea) -> &'static str {
    match area {
        crate::core::LinkSourceArea::Body => "body",
        crate::core::LinkSourceArea::Frontmatter => "frontmatter",
    }
}

fn unresolved_reason_str(reason: &crate::core::UnresolvedReason) -> &'static str {
    match reason {
        crate::core::UnresolvedReason::TargetMissing => "target-missing",
        crate::core::UnresolvedReason::AnchorMissing => "anchor-missing",
        crate::core::UnresolvedReason::BlockRefMissing => "block-ref-missing",
        crate::core::UnresolvedReason::Ambiguous => "ambiguous",
    }
}

fn insert_link(tx: &rusqlite::Transaction, link: &Link) -> Result<(), CacheError> {
    let kind = link_kind_str(&link.kind);
    let resolved = link.resolved_path.as_ref().map(|p| p.as_str().to_string());
    let status = link_status_str(&link.status);
    let source_context = link
        .source_context
        .as_ref()
        .map(|c| link_source_area_str(&c.area).to_string());
    let source_context_property = link
        .source_context
        .as_ref()
        .and_then(|c| c.property.clone());
    // SourceSpan currently exposes only a single byte offset; store it as
    // span_start and leave span_end NULL until the parser tracks an end.
    // Line/column are persisted in their own columns so the cache round-trip
    // matches `crate::graph::build_index` for downstream consumers that read
    // those fields.
    let (span_start, span_end, span_line, span_column): (
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    ) = match &link.source_span {
        Some(s) => (
            Some(s.byte_offset as i64),
            None,
            Some(s.line as i64),
            Some(s.column as i64),
        ),
        None => (None, None, None, None),
    };
    let unresolved_reason = link.unresolved_reason.as_ref().map(unresolved_reason_str);
    let candidates_json = if link.candidates.is_empty() {
        None
    } else {
        // Serialize candidate paths as a JSON array of strings. Read-side
        // parses with serde_json; failure round-trips as an empty list.
        Some(serde_json::to_string(&link.candidates).unwrap_or_default())
    };
    tx.execute(
        "INSERT INTO links
           (source_path, raw, kind, target_raw, resolved_path, anchor, block_ref,
            label, source_span_start, source_span_end, source_span_line, source_span_column,
            source_context, source_context_property, status, unresolved_reason, candidates_json)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            link.source_path.as_str(),
            link.raw,
            kind,
            link.target,
            resolved,
            link.anchor,
            link.block_ref,
            link.label,
            span_start,
            span_end,
            span_line,
            span_column,
            source_context,
            source_context_property,
            status,
            unresolved_reason,
            candidates_json,
        ],
    )?;
    Ok(())
}

fn insert_file(
    tx: &rusqlite::Transaction,
    vault_root: &Utf8Path,
    file: &VaultFile,
) -> Result<(), CacheError> {
    let ext = file.extension.as_deref().unwrap_or("");
    let absolute = vault_root.join(&file.path);
    let size = size_bytes(&absolute).unwrap_or(0);
    let mtime = mtime_ns(&absolute).unwrap_or(0);
    tx.execute(
        "INSERT OR REPLACE INTO files (path, ext, size_bytes, mtime_ns) VALUES (?, ?, ?, ?)",
        params![file.path.as_str(), ext, size, mtime],
    )?;
    Ok(())
}

fn update_meta_rebuild_ts(tx: &rusqlite::Transaction) -> Result<(), CacheError> {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string();
    tx.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('last_full_rebuild_ts', ?)",
        params![now_secs],
    )?;
    Ok(())
}

/// Stamp the `links_alias_field` meta row with the value the cache was
/// opened with. Always written (empty string when alias support is disabled)
/// so subsequent `Cache::open_with_config` calls can compare against a
/// definite value.
fn update_meta_alias_field(
    tx: &rusqlite::Transaction,
    alias_field: Option<&str>,
) -> Result<(), CacheError> {
    tx.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('links_alias_field', ?)",
        params![alias_field.unwrap_or("")],
    )?;
    Ok(())
}

/// Stamp the `index_set_hash` meta row so a later `Cache::open_with_index`
/// call can tell whether `document_fields` already matches the resolved
/// Wave-2 index set, or needs a re-shred.
fn update_meta_index_set_hash(tx: &rusqlite::Transaction, hash: &str) -> Result<(), CacheError> {
    tx.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('index_set_hash', ?)",
        params![hash],
    )?;
    Ok(())
}

fn mtime_ns(path: &Utf8Path) -> Option<i64> {
    std::fs::metadata(path.as_std_path()).ok().and_then(|m| {
        m.modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_nanos() as i64)
    })
}

fn size_bytes(path: &Utf8Path) -> Option<i64> {
    std::fs::metadata(path.as_std_path())
        .ok()
        .map(|m| m.len() as i64)
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    fn make_vault_with_one_doc() -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        // Create the vault under a non-hidden subdirectory: TempDir's own
        // basename starts with `.tmp`, which vault_graph's WalkDir filter
        // treats as hidden and skips entirely.
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(
            root.join("doc.md").as_std_path(),
            "---\ntitle: Doc\n---\n# Heading\n\nbody [link](other.md)\n",
        )
        .unwrap();
        std::fs::write(
            root.join("other.md").as_std_path(),
            "---\ntitle: Other\n---\n",
        )
        .unwrap();
        (tmp, root)
    }

    /// Vault with an `Archive/` subdir (a *visible* path — the stock ignore
    /// entries are all hidden dirs the scanner skips anyway) plus a live doc
    /// that links `[[archived]]`.
    fn make_vault_with_archive() -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::create_dir(root.join("Archive").as_std_path()).unwrap();
        std::fs::write(
            root.join("Archive/archived.md").as_std_path(),
            "---\ntitle: Archived\n---\n",
        )
        .unwrap();
        std::fs::write(
            root.join("live.md").as_std_path(),
            "---\ntitle: Live\n---\n\nsee [[archived]]\n",
        )
        .unwrap();
        (tmp, root)
    }

    fn doc_count_under(cache: &crate::cache::Cache, prefix: &str) -> i64 {
        cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM documents WHERE path LIKE ?",
                [format!("{prefix}%")],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn files_ignore_excludes_docs_at_rebuild() {
        // With Archive/** in files.ignore, a full rebuild must never index the
        // archived doc — not present in the documents table, not queryable,
        // not a link target (NRN-117).
        let (_tmp, root) = make_vault_with_archive();
        let ignore = vec!["Archive/**".to_string()];
        let mut cache = crate::cache::Cache::open_with_index(
            &root,
            None,
            &ignore,
            &std::collections::BTreeSet::new(),
            "test-hash",
        )
        .unwrap();
        cache.rebuild(&root).unwrap();

        assert_eq!(
            doc_count_under(&cache, "Archive/"),
            0,
            "archived docs must be excluded from the cache"
        );
        assert_eq!(doc_count_under(&cache, "live.md"), 1, "live doc stays");

        // The link [[archived]] into the now-ignored target must be unresolved
        // (link-target-missing), not silently resolved to the excluded doc.
        let resolved: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM links WHERE resolved_path LIKE 'Archive/%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(resolved, 0, "no link may resolve into an ignored path");
    }

    #[test]
    fn files_ignore_purges_newly_ignored_on_incremental() {
        // Build WITHOUT ignore (archived doc indexed), then reopen WITH ignore
        // and run an incremental refresh: the newly-ignored doc must be purged,
        // matching a full rebuild (NRN-117 / determinism).
        let (_tmp, root) = make_vault_with_archive();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();
        assert_eq!(doc_count_under(&cache, "Archive/"), 1, "indexed initially");
        drop(cache);

        let ignore = vec!["Archive/**".to_string()];
        let mut cache = crate::cache::Cache::open_with_index(
            &root,
            None,
            &ignore,
            &std::collections::BTreeSet::new(),
            "test-hash",
        )
        .unwrap();
        cache
            .index_incremental(&root, &crate::cache::ChangeDetectOptions::default())
            .unwrap();
        assert_eq!(
            doc_count_under(&cache, "Archive/"),
            0,
            "incremental refresh must purge the newly-ignored doc"
        );
    }

    #[test]
    fn incremental_refresh_matches_rebuild_after_alias_change() {
        // a.md links [[foo]]; b.md initially has no matching alias, so [[foo]] is
        // unresolved. Adding alias `foo` to b.md must make [[foo]] resolve on the
        // NEXT incremental refresh, identically to a full rebuild — link
        // resolution is global, so a change to b's aliases re-resolves a's link
        // even though a itself did not change (NRN-126, the determinism rule).
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(
            root.join("a.md").as_std_path(),
            "---\ntitle: A\n---\n\nsee [[foo]]\n",
        )
        .unwrap();
        std::fs::write(
            root.join("b.md").as_std_path(),
            "---\ntitle: B\n---\n\nB body\n",
        )
        .unwrap();

        let resolved = |cache: &crate::cache::Cache| -> Option<String> {
            cache
                .conn
                .query_row(
                    "SELECT resolved_path FROM links WHERE source_path = 'a.md'",
                    [],
                    |r| r.get::<_, Option<String>>(0),
                )
                .unwrap()
        };

        // Build with alias_field configured so aliases participate in resolution.
        let mut cache = crate::cache::Cache::open_with_index(
            &root,
            Some("aliases"),
            &[],
            &std::collections::BTreeSet::new(),
            "hash",
        )
        .unwrap();
        cache.rebuild(&root).unwrap();
        assert_eq!(
            resolved(&cache),
            None,
            "[[foo]] unresolved before the alias"
        );

        // Add alias `foo` to b.md on disk, then run an incremental refresh.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(
            root.join("b.md").as_std_path(),
            "---\ntitle: B\naliases:\n  - foo\n---\n\nB body\n",
        )
        .unwrap();
        cache
            .index_incremental(&root, &crate::cache::ChangeDetectOptions::default())
            .unwrap();
        let incremental = resolved(&cache);

        // A full rebuild is the determinism oracle.
        cache.rebuild(&root).unwrap();
        let rebuilt = resolved(&cache);

        assert_eq!(
            incremental, rebuilt,
            "incremental link resolution must equal a full rebuild"
        );
        assert_eq!(
            rebuilt,
            Some("b.md".to_string()),
            "[[foo]] should resolve to b.md via its new alias"
        );
    }

    #[test]
    fn rebuild_populates_documents_table() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        let report = cache.rebuild(&root).unwrap();
        assert_eq!(report.doc_count, 2);

        let count: i64 = cache
            .conn
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn rebuild_populates_links_table() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        let count: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM links WHERE source_path = 'doc.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn rebuild_stores_body_text() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        let body: String = cache
            .conn
            .query_row(
                "SELECT body_text FROM documents WHERE path = 'doc.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(body.contains("# Heading"));
        assert!(body.contains("body [link](other.md)"));
        // Frontmatter not in body_text.
        assert!(!body.contains("title: Doc"));
    }

    #[test]
    fn incremental_picks_up_added_file() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        std::fs::write(
            root.join("third.md").as_std_path(),
            "---\ntitle: Third\n---\n",
        )
        .unwrap();
        let report = cache.index_incremental(&root, &Default::default()).unwrap();
        assert!(report.doc_count >= 1);

        let count: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM documents WHERE path = 'third.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn incremental_removes_deleted_file() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        std::fs::remove_file(root.join("other.md").as_std_path()).unwrap();
        cache.index_incremental(&root, &Default::default()).unwrap();

        let count: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM documents WHERE path = 'other.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);

        // Links targeting other.md should now be unresolved.
        let resolved: Option<String> = cache
            .conn
            .query_row(
                "SELECT resolved_path FROM links WHERE source_path = 'doc.md' AND target_raw = 'other.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(resolved.is_none());
    }

    #[test]
    fn incremental_after_no_changes_is_cheap() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();
        let report = cache.index_incremental(&root, &Default::default()).unwrap();
        assert_eq!(report.doc_count, 0);
        assert_eq!(report.file_count, 0);
    }

    #[test]
    fn incremental_handles_rename_via_delete_plus_add() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        std::fs::rename(
            root.join("other.md").as_std_path(),
            root.join("renamed.md").as_std_path(),
        )
        .unwrap();
        cache.index_incremental(&root, &Default::default()).unwrap();

        let other_count: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM documents WHERE path = 'other.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(other_count, 0);
        let renamed_count: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM documents WHERE path = 'renamed.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(renamed_count, 1);
    }

    #[test]
    fn cache_rebuild_threads_alias_field_through_to_link_resolution() {
        // Regression: prior to this fix, `Cache::rebuild` called
        // `crate::graph::build_index` with default options, which left
        // `alias_field = None` so alias fallback never ran during link
        // resolution. Cached link rows then served alias-blind status to
        // every downstream consumer (validate, find, repair plan, show
        // incoming).
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        // vm.md is the alias target.
        std::fs::write(
            root.join("vm.md").as_std_path(),
            "---\naliases:\n  - Vault Memory\n---\n# Vault Memory\n",
        )
        .unwrap();
        // src.md has a wikilink that only resolves via alias.
        std::fs::write(
            root.join("src.md").as_std_path(),
            "---\n---\n# Source\n\nThis links to [[Vault Memory]].\n",
        )
        .unwrap();

        let mut cache = crate::cache::Cache::open_with_config(&root, Some("aliases")).unwrap();
        cache.rebuild(&root).unwrap();

        let deep = cache
            .document_with_connections(camino::Utf8Path::new("src.md"), false)
            .unwrap()
            .expect("src.md in cache");
        // After rebuild with alias_field set, the [[Vault Memory]] link in
        // src.md must be Resolved (via alias) — NOT Unresolved.
        assert!(
            deep.unresolved_links.is_empty(),
            "expected no unresolved links, got: {:?}",
            deep.unresolved_links
        );
        let alias_link = deep
            .outgoing_links
            .iter()
            .find(|l| l.target == "Vault Memory")
            .expect("[[Vault Memory]] outgoing link");
        assert_eq!(
            alias_link.resolved_path.as_deref(),
            Some(camino::Utf8Path::new("vm.md")),
            "alias link should resolve to vm.md"
        );
    }

    #[test]
    fn rebuild_clears_existing_rows() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();
        // Add a stale row.
        cache
            .conn
            .execute(
                "INSERT INTO documents (path, stem, hash, body_text, mtime_ns, size_bytes) \
                 VALUES ('stale.md', 'stale', 'h', 'b', 0, 0)",
                [],
            )
            .unwrap();
        cache.rebuild(&root).unwrap();
        let count: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM documents WHERE path = 'stale.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    // Concurrency integration test: two simultaneous `rebuild` calls must
    // both complete successfully, with the second one serializing behind
    // the first via the advisory write lock.
    #[test]
    fn two_simultaneous_rebuilds_serialize() {
        let tmp = TempDir::new().unwrap();
        // vault_graph treats hidden directories (basename starts with `.`) as
        // skipped — TempDir's own basename starts with `.tmp`, so nest the
        // vault under a non-hidden subdirectory.
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(
            root.join("a.md").as_std_path(),
            "---\ntitle: A\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            root.join("b.md").as_std_path(),
            "---\ntitle: B\n---\nbody [[a]]\n",
        )
        .unwrap();

        let root1 = root.clone();
        let handle1 = std::thread::spawn(move || {
            let mut cache = crate::cache::Cache::open(&root1).unwrap();
            cache.rebuild(&root1)
        });

        // Tiny stagger so handle1 has reached `rebuild` and acquired the lock
        // before handle2 races for it. Without this the test still asserts both
        // succeed, but with the stagger we exercise the "second writer waits"
        // path deterministically.
        std::thread::sleep(std::time::Duration::from_millis(10));

        let root2 = root.clone();
        let handle2 = std::thread::spawn(move || {
            let mut cache = crate::cache::Cache::open(&root2).unwrap();
            cache.rebuild(&root2)
        });

        let r1 = handle1.join().unwrap();
        let r2 = handle2.join().unwrap();
        assert!(r1.is_ok(), "first rebuild failed: {r1:?}");
        assert!(r2.is_ok(), "second rebuild failed: {r2:?}");
    }
}
