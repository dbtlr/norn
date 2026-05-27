//! Deep document projection — `Cache::document_with_connections`.
//!
//! Returns a single document populated with its headings, outgoing links
//! (resolved), unresolved links, and incoming links (other documents that
//! link *to* this one).  Used by `vault show`.

use crate::core::{Heading, Link, LinkStatus};
use camino::{Utf8Path, Utf8PathBuf};
use rusqlite::OptionalExtension;
use serde::Serialize;

use crate::cache::error::CacheError;

/// A document with full connection context: headings, outgoing links,
/// unresolved links, and incoming back-links.
#[derive(Debug, Serialize)]
pub struct DocumentDeep {
    pub path: Utf8PathBuf,
    pub frontmatter: Option<serde_json::Value>,
    pub headings: Vec<Heading>,
    pub outgoing_links: Vec<Link>,
    pub unresolved_links: Vec<Link>,
    pub incoming_links: Vec<IncomingLink>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// A back-link: another document's path plus the `Link` record that points
/// at the current document.
#[derive(Debug, Serialize)]
pub struct IncomingLink {
    pub source_path: Utf8PathBuf,
    pub link: Link,
}

impl crate::cache::Cache {
    /// Load a single document with its full connection set.
    ///
    /// - `outgoing_links` — links with `LinkStatus::Resolved`
    /// - `unresolved_links` — links with any other status
    /// - `incoming_links` — links from other documents whose `resolved_path`
    ///   equals `path`
    /// - `body` — the raw body text, only populated when `with_body` is `true`
    ///
    /// Returns `None` if `path` is not present in the cache.
    pub fn document_with_connections(
        &self,
        path: &Utf8Path,
        with_body: bool,
    ) -> Result<Option<DocumentDeep>, CacheError> {
        let mut stmt = self.conn.prepare(
            "SELECT path, frontmatter_json, body_text \
             FROM documents WHERE path = ?",
        )?;
        let row = stmt
            .query_row([path.as_str()], |row| {
                let path: String = row.get(0)?;
                let frontmatter_json: Option<String> = row.get(1)?;
                let body_text: String = row.get(2)?;
                Ok((path, frontmatter_json, body_text))
            })
            .optional()?;

        let Some((path_str, fm_json, body_text)) = row else {
            return Ok(None);
        };

        let frontmatter = fm_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok());
        let path_buf = Utf8PathBuf::from(&path_str);

        let headings = crate::cache::reader::load_headings(&self.conn, &path_str)?;
        let all_links = crate::cache::reader::load_links(&self.conn, &path_str)?;

        let mut outgoing_links = Vec::new();
        let mut unresolved_links = Vec::new();
        for link in all_links {
            if link.status == LinkStatus::Resolved {
                outgoing_links.push(link);
            } else {
                unresolved_links.push(link);
            }
        }

        let incoming_links = load_incoming(&self.conn, &path_buf)?;

        Ok(Some(DocumentDeep {
            path: path_buf,
            frontmatter,
            headings,
            outgoing_links,
            unresolved_links,
            incoming_links,
            body: with_body.then_some(body_text),
        }))
    }
}

/// Load all links whose `resolved_path` equals `target_path` — i.e. back-links
/// pointing at the current document.  Mirrors `reader::load_links` but filters
/// by `resolved_path` instead of `source_path`.
fn load_incoming(
    conn: &rusqlite::Connection,
    target_path: &Utf8Path,
) -> Result<Vec<IncomingLink>, CacheError> {
    let mut stmt = conn.prepare(
        "SELECT source_path, raw, kind, target_raw, resolved_path, anchor, block_ref, label,
                source_span_start, source_span_end, source_span_line, source_span_column,
                source_context, source_context_property, status, unresolved_reason,
                candidates_json
         FROM links WHERE resolved_path = ?
         ORDER BY source_path, source_span_start",
    )?;

    let rows = stmt.query_map([target_path.as_str()], |row| {
        crate::cache::query_links::decode_link_row(row)
    })?;

    let mut out: Vec<IncomingLink> = Vec::new();
    for row in rows {
        let link = row?;
        out.push(IncomingLink {
            source_path: link.source_path.clone(),
            link,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    fn synth() -> (TempDir, Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("vault-cache-show-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(
            root.join("a.md").as_std_path(),
            "---\ntype: note\n---\n# A heading\n[[b]]\n",
        )
        .unwrap();
        std::fs::write(
            root.join("b.md").as_std_path(),
            "---\ntype: note\n---\n# B heading\n[[a]]\n",
        )
        .unwrap();
        (tmp, root)
    }

    #[test]
    fn deep_projection_includes_headings_outgoing_incoming() {
        let (_t, root) = synth();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();
        let deep = cache
            .document_with_connections(camino::Utf8Path::new("a.md"), false)
            .unwrap()
            .unwrap();
        assert_eq!(deep.path.as_str(), "a.md");
        assert!(deep.headings.iter().any(|h| h.text == "A heading"));
        assert!(deep.outgoing_links.iter().any(|l| l.target == "b"));
        assert!(deep
            .incoming_links
            .iter()
            .any(|il| il.source_path.as_str() == "b.md"));
        assert!(deep.body.is_none());
    }

    #[test]
    fn body_only_loaded_when_requested() {
        let (_t, root) = synth();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();
        let deep = cache
            .document_with_connections(camino::Utf8Path::new("a.md"), true)
            .unwrap()
            .unwrap();
        assert!(deep.body.as_ref().unwrap().contains("A heading"));
    }

    #[test]
    fn missing_path_returns_none() {
        let (_t, root) = synth();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();
        let deep = cache
            .document_with_connections(camino::Utf8Path::new("nonexistent.md"), false)
            .unwrap();
        assert!(deep.is_none());
    }

    #[test]
    fn document_with_connections_uses_index_for_incoming_links() {
        let (_t, root) = synth();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        // The incoming-link query is the load-bearing one for perf:
        // without an index on `resolved_path`, this would be a full scan
        // of the links table, executed per show target. Lock the
        // invariant: the plan MUST use idx_links_resolved (or another
        // index — substring "USING INDEX" is sufficient evidence).
        let conn = cache.conn();
        let mut stmt = conn
            .prepare("EXPLAIN QUERY PLAN SELECT source_path FROM links WHERE resolved_path = ?")
            .unwrap();
        let plan_rows: Vec<String> = stmt
            .query_map(["a.md"], |row| row.get::<_, String>(3))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        for row in &plan_rows {
            assert!(
                !row.starts_with("SCAN") || row.contains("USING INDEX"),
                "incoming-link query plan should not be a full scan: {}",
                row
            );
        }
    }
}
