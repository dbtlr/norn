//! Link queries — `Cache::links_unresolved` and `Cache::backlinks_to`.
//! Share row-decoding logic but with different WHERE clauses.

use camino::{Utf8Path, Utf8PathBuf};
use rusqlite::params_from_iter;
use rusqlite::types::Value as SqlValue;
use vault_core::{
    Link, LinkKind, LinkSourceArea, LinkSourceContext, LinkStatus, SourceSpan, UnresolvedReason,
};

use crate::error::CacheError;

impl crate::Cache {
    /// Every link with status != Resolved. Used by `vault links unresolved`.
    pub fn links_unresolved(&self) -> Result<Vec<Link>, CacheError> {
        query_links(
            &self.conn,
            "WHERE status <> ?",
            vec![SqlValue::Text("resolved".into())],
        )
    }

    /// Every link with resolved_path == path. Used by `vault links backlinks`.
    pub fn backlinks_to(&self, path: &Utf8Path) -> Result<Vec<Link>, CacheError> {
        query_links(
            &self.conn,
            "WHERE resolved_path = ?",
            vec![SqlValue::Text(path.as_str().to_string())],
        )
    }
}

fn query_links(
    conn: &rusqlite::Connection,
    where_clause: &str,
    binds: Vec<SqlValue>,
) -> Result<Vec<Link>, CacheError> {
    let sql = format!(
        "SELECT source_path, raw, kind, target_raw, resolved_path, anchor, block_ref, label, \
                source_span_start, source_span_end, source_span_line, source_span_column, \
                source_context, source_context_property, status, unresolved_reason, candidates_json \
         FROM links {} ORDER BY source_path, rowid",
        where_clause
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(binds.iter()), decode_link_row)?;
    let mut links = Vec::new();
    for row in rows {
        links.push(row?);
    }
    Ok(links)
}

fn decode_link_row(row: &rusqlite::Row) -> rusqlite::Result<Link> {
    let source_path: String = row.get(0)?;
    let raw: String = row.get(1)?;
    let kind_str: String = row.get(2)?;
    let target: String = row.get(3)?;
    let resolved: Option<String> = row.get(4)?;
    let anchor: Option<String> = row.get(5)?;
    let block_ref: Option<String> = row.get(6)?;
    let label: Option<String> = row.get(7)?;
    let span_start: Option<i64> = row.get(8)?;
    let _span_end: Option<i64> = row.get(9)?;
    let span_line: Option<i64> = row.get(10)?;
    let span_column: Option<i64> = row.get(11)?;
    let context_str: Option<String> = row.get(12)?;
    let context_property: Option<String> = row.get(13)?;
    let status_str: String = row.get(14)?;
    let unresolved_reason_str: Option<String> = row.get(15)?;
    let candidates_json: Option<String> = row.get(16)?;

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

    Ok(Link {
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
    })
}
