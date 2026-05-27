//! Read-only SQL primitives that power the LIVE EXAMPLES generator for
//! `vault find --help`. Aggregates top-level frontmatter field statistics
//! and counts documents matching a conjunction of predicates.

use std::collections::{BTreeMap, BTreeSet};

use crate::cache::error::CacheError;
use crate::cache::Cache;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldStats {
    pub field: String,
    pub docs_with_field: usize,
    pub distinct_values: usize,
    pub is_all_scalar_string: bool,
    pub top_value: String,
    pub top_value_doc_count: usize,
}

pub fn field_statistics(cache: &Cache) -> Result<Vec<FieldStats>, CacheError> {
    // Aggregate per-(field, value) doc counts in Rust. The frontmatter JSON
    // payloads are small (top-level object only) and vault sizes are bounded
    // (~1k docs typical), so a single-pass scan + serde_json parse is faster
    // than trying to coerce SQLite's json_each into a cross-join with reliable
    // type handling.
    let mut stmt = cache
        .conn()
        .prepare("SELECT frontmatter_json FROM documents WHERE frontmatter_json IS NOT NULL")?;

    #[derive(Default)]
    struct Acc {
        docs_with_field: usize,
        all_scalar_string: bool,
        // value -> doc count (only populated for scalar-string values).
        string_values: BTreeMap<String, usize>,
        // distinct non-string values seen, keyed by json-stringified form.
        non_string_values: BTreeSet<String>,
    }
    let mut by_field: BTreeMap<String, Acc> = BTreeMap::new();

    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let raw: String = row.get(0)?;
        let parsed: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let obj = match parsed.as_object() {
            Some(o) => o,
            None => continue,
        };
        for (field, value) in obj.iter() {
            let entry = by_field.entry(field.clone()).or_insert_with(|| Acc {
                docs_with_field: 0,
                all_scalar_string: true,
                string_values: BTreeMap::new(),
                non_string_values: BTreeSet::new(),
            });
            entry.docs_with_field += 1;
            if let serde_json::Value::String(s) = value {
                *entry.string_values.entry(s.clone()).or_insert(0) += 1;
            } else {
                entry.all_scalar_string = false;
                entry.non_string_values.insert(value.to_string());
            }
        }
    }

    Ok(by_field
        .into_iter()
        .map(|(field, acc)| {
            // Top scalar-string value — highest count, alphabetical tiebreak.
            let (top_value, top_count) = acc
                .string_values
                .iter()
                .max_by(|(av, ac), (bv, bc)| ac.cmp(bc).then_with(|| bv.cmp(av)))
                .map(|(v, c)| (v.clone(), *c))
                .unwrap_or_default();
            let distinct_values = acc.string_values.len() + acc.non_string_values.len();
            FieldStats {
                field,
                docs_with_field: acc.docs_with_field,
                distinct_values,
                is_all_scalar_string: acc.all_scalar_string,
                top_value,
                top_value_doc_count: top_count,
            }
        })
        .collect())
}

pub fn count_matching(cache: &Cache, predicates: &[(&str, &str)]) -> Result<usize, CacheError> {
    if predicates.is_empty() {
        return Ok(0);
    }
    let mut sql = String::from("SELECT COUNT(*) FROM documents WHERE frontmatter_json IS NOT NULL");
    let mut binds: Vec<String> = Vec::with_capacity(predicates.len());
    for (field, value) in predicates {
        // Field name is interpolated into the JSON path; values are bound.
        // Validate the field name as a strict JSON key to keep injection-safe.
        if !field
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Ok(0);
        }
        sql.push_str(&format!(
            " AND json_extract(frontmatter_json, '$.{field}') = ?"
        ));
        binds.push((*value).to_string());
    }
    let count: i64 =
        cache
            .conn()
            .query_row(&sql, rusqlite::params_from_iter(binds.iter()), |r| r.get(0))?;
    Ok(count.max(0) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use rusqlite::params;
    use tempfile::TempDir;

    fn fresh_cache() -> (TempDir, Cache) {
        let tmp = tempfile::Builder::new()
            .prefix("vault-cli-live-examples-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let cache = Cache::open(&root).unwrap();
        (tmp, cache)
    }

    fn insert_doc(cache: &Cache, path: &str, frontmatter_json: &str) {
        cache
            .conn()
            .execute(
                "INSERT INTO documents (path, stem, hash, frontmatter_json, body_text, mtime_ns, size_bytes) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![path, path.trim_end_matches(".md"), "h", frontmatter_json, "", 0i64, 0i64],
            )
            .unwrap();
    }

    #[test]
    fn empty_cache_returns_no_stats() {
        let (_tmp, cache) = fresh_cache();
        let stats = field_statistics(&cache).unwrap();
        assert!(stats.is_empty());
    }

    #[test]
    fn captures_distinct_values_and_top() {
        let (_tmp, cache) = fresh_cache();
        insert_doc(&cache, "a.md", r#"{"type":"note"}"#);
        insert_doc(&cache, "b.md", r#"{"type":"note"}"#);
        insert_doc(&cache, "c.md", r#"{"type":"task"}"#);
        let stats = field_statistics(&cache).unwrap();
        assert_eq!(stats.len(), 1);
        let s = &stats[0];
        assert_eq!(s.field, "type");
        assert_eq!(s.docs_with_field, 3);
        assert_eq!(s.distinct_values, 2);
        assert!(s.is_all_scalar_string);
        assert_eq!(s.top_value, "note");
        assert_eq!(s.top_value_doc_count, 2);
    }

    #[test]
    fn alphabetical_tiebreak_on_equal_counts() {
        let (_tmp, cache) = fresh_cache();
        insert_doc(&cache, "a.md", r#"{"status":"backlog"}"#);
        insert_doc(&cache, "b.md", r#"{"status":"done"}"#);
        let stats = field_statistics(&cache).unwrap();
        let s = stats.iter().find(|s| s.field == "status").unwrap();
        assert_eq!(s.top_value, "backlog");
    }

    #[test]
    fn array_value_flags_not_scalar_string() {
        let (_tmp, cache) = fresh_cache();
        insert_doc(&cache, "a.md", r#"{"aliases":["foo","bar"]}"#);
        let stats = field_statistics(&cache).unwrap();
        let s = stats.iter().find(|s| s.field == "aliases").unwrap();
        assert!(!s.is_all_scalar_string);
    }

    #[test]
    fn number_value_flags_not_scalar_string() {
        let (_tmp, cache) = fresh_cache();
        insert_doc(&cache, "a.md", r#"{"priority":1}"#);
        let stats = field_statistics(&cache).unwrap();
        let s = stats.iter().find(|s| s.field == "priority").unwrap();
        assert!(!s.is_all_scalar_string);
    }

    #[test]
    fn mixed_string_and_array_flags_not_scalar_string() {
        let (_tmp, cache) = fresh_cache();
        insert_doc(&cache, "a.md", r#"{"tags":"single"}"#);
        insert_doc(&cache, "b.md", r#"{"tags":["multi","tag"]}"#);
        let stats = field_statistics(&cache).unwrap();
        let s = stats.iter().find(|s| s.field == "tags").unwrap();
        assert!(!s.is_all_scalar_string);
    }

    #[test]
    fn count_matching_zero_predicates_returns_zero() {
        let (_tmp, cache) = fresh_cache();
        insert_doc(&cache, "a.md", r#"{"type":"note"}"#);
        assert_eq!(count_matching(&cache, &[]).unwrap(), 0);
    }

    #[test]
    fn count_matching_single_predicate() {
        let (_tmp, cache) = fresh_cache();
        insert_doc(&cache, "a.md", r#"{"type":"note"}"#);
        insert_doc(&cache, "b.md", r#"{"type":"note"}"#);
        insert_doc(&cache, "c.md", r#"{"type":"task"}"#);
        let count = count_matching(&cache, &[("type", "note")]).unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn count_matching_two_predicates_intersect() {
        let (_tmp, cache) = fresh_cache();
        insert_doc(&cache, "a.md", r#"{"type":"note","workspace":"vault-cli"}"#);
        insert_doc(&cache, "b.md", r#"{"type":"note","workspace":"atlas"}"#);
        insert_doc(&cache, "c.md", r#"{"type":"task","workspace":"vault-cli"}"#);
        let count =
            count_matching(&cache, &[("type", "note"), ("workspace", "vault-cli")]).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn count_matching_rejects_unsafe_field_name() {
        let (_tmp, cache) = fresh_cache();
        insert_doc(&cache, "a.md", r#"{"type":"note"}"#);
        let count = count_matching(&cache, &[("type'); DROP TABLE documents; --", "x")]).unwrap();
        assert_eq!(count, 0);
    }
}
