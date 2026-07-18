//! Live-examples generator for `norn find --help`.
//!
//! Composition algorithm (Phase 3 spec §4):
//! 1. Collect per-field statistics from the cache.
//! 2. Filter for enum-like fields (scalar string, bounded cardinality,
//!    meaningful top-value coverage).
//! 3. Rank by `coverage * top-value-share` descending, alphabetical tiebreak.
//! 4. Predicate 1 = top-ranked field + its top value.
//! 5. Walk the remainder; pick the first field whose `(F1:V1 AND Fk:Vk)`
//!    intersection has ≥ 1 doc as predicate 2. If none, single-predicate.
//! 6. Pick a date-like sort field by name preference.
//! 7. Compose `BIN_NAME find --eq F1:V1 [--eq F2:V2] [--sort SORT] --limit 5`.
//! 8. Re-count the final query; emit only if non-zero.

use crate::cache::{count_matching, field_statistics, Cache, FieldStats};

use crate::help::bin_name::BIN_NAME;
use crate::help::model::LiveExample;

const ENUM_MAX_DISTINCT: usize = 20;
const ENUM_MIN_TOP_DOCS: usize = 3;
const FIND_LIMIT: usize = 5;

/// Preference-ordered date-like field names.
const DATE_LIKE_FIELDS: &[&str] = &["modified", "created", "updated", "published", "date"];

/// Public entry point — wired into `examples::live_examples_fn_for("norn find")`.
pub fn live_examples_for_find(cache: &Cache) -> Vec<LiveExample> {
    let Ok(stats) = field_statistics(cache) else {
        return Vec::new();
    };
    let enum_like = filter_and_rank_enum_like(&stats);
    if enum_like.is_empty() {
        return Vec::new();
    }
    let p1 = (enum_like[0].field.as_str(), enum_like[0].top_value.as_str());
    let p2 = pick_second_predicate(cache, &enum_like, p1);
    let sort = pick_date_like_field(&stats);
    let query = compose_query(p1, p2, sort);

    // Re-count the final query for the rendered tail. With p2 we already
    // verified ≥ 1 in pick_second_predicate; with single-predicate, the
    // enum-like filter already guaranteed ≥ ENUM_MIN_TOP_DOCS. Re-count is
    // the source of truth for the rendered count.
    let mut preds = vec![p1];
    if let Some(p2) = p2 {
        preds.push(p2);
    }
    let count = count_matching(cache, &preds).unwrap_or(0);
    if count == 0 {
        return Vec::new();
    }
    vec![LiveExample {
        query,
        match_count: count,
    }]
}

fn filter_and_rank_enum_like(stats: &[FieldStats]) -> Vec<FieldStats> {
    let mut enum_like: Vec<FieldStats> = stats
        .iter()
        .filter(|s| {
            s.is_all_scalar_string
                && s.distinct_values <= ENUM_MAX_DISTINCT
                && s.top_value_doc_count >= ENUM_MIN_TOP_DOCS
                && s.top_value_doc_count.saturating_mul(10) >= s.docs_with_field
                && !s.top_value.chars().any(char::is_whitespace)
        })
        .cloned()
        .collect();
    enum_like.sort_by(|a, b| {
        let ka = ranking_key(a);
        let kb = ranking_key(b);
        // Descending on ranking key; alphabetical on field name tiebreak.
        kb.partial_cmp(&ka)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.field.cmp(&b.field))
    });
    enum_like
}

fn ranking_key(s: &FieldStats) -> f64 {
    if s.docs_with_field == 0 {
        return 0.0;
    }
    let coverage = s.docs_with_field as f64;
    let top_share = s.top_value_doc_count as f64 / s.docs_with_field as f64;
    coverage * top_share
}

fn pick_second_predicate<'a>(
    cache: &Cache,
    ranked: &'a [FieldStats],
    p1: (&str, &str),
) -> Option<(&'a str, &'a str)> {
    for cand in ranked.iter().skip(1) {
        let preds = [(p1.0, p1.1), (cand.field.as_str(), cand.top_value.as_str())];
        if count_matching(cache, &preds).unwrap_or(0) >= 1 {
            return Some((cand.field.as_str(), cand.top_value.as_str()));
        }
    }
    None
}

fn pick_date_like_field(stats: &[FieldStats]) -> Option<&'static str> {
    for name in DATE_LIKE_FIELDS {
        if stats.iter().any(|s| s.field == *name) {
            return Some(*name);
        }
    }
    None
}

fn compose_query(p1: (&str, &str), p2: Option<(&str, &str)>, sort: Option<&str>) -> String {
    let mut s = format!("{BIN_NAME} find --eq {}:{}", p1.0, strip_wikilink(p1.1));
    if let Some((f, v)) = p2 {
        s.push_str(&format!(" --eq {f}:{}", strip_wikilink(v)));
    }
    if let Some(sf) = sort {
        s.push_str(&format!(" --sort {sf}"));
    }
    s.push_str(&format!(" --limit {FIND_LIMIT}"));
    s
}

/// Strip `[[…]]` wikilink brackets from a value for rendering. The CLI's
/// bracket-tolerant matcher accepts the bare form, and brackets would
/// otherwise require shell escaping when a user copy-pastes the example.
/// Returns the input unchanged when it isn't wikilink-shaped.
fn strip_wikilink(value: &str) -> &str {
    value
        .strip_prefix("[[")
        .and_then(|v| v.strip_suffix("]]"))
        .unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Cache;
    use camino::Utf8PathBuf;
    use rusqlite::params;
    use tempfile::TempDir;

    fn fresh_cache() -> (TempDir, Cache) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-find-live-")
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
    fn empty_vault_returns_no_examples() {
        let (_tmp, cache) = fresh_cache();
        assert!(live_examples_for_find(&cache).is_empty());
    }

    #[test]
    fn vault_with_no_enum_like_fields_returns_no_examples() {
        let (_tmp, cache) = fresh_cache();
        // Two docs, each with a unique `title` and a unique date.
        insert_doc(
            &cache,
            "a.md",
            r#"{"title":"Alpha","created":"2026-01-01"}"#,
        );
        insert_doc(&cache, "b.md", r#"{"title":"Beta","created":"2026-02-01"}"#);
        // `title` distinct_values=2 BUT top covers 1 doc < 3 → not enum-like.
        assert!(live_examples_for_find(&cache).is_empty());
    }

    #[test]
    fn enum_field_below_top_count_floor_excluded() {
        let (_tmp, cache) = fresh_cache();
        // 4 docs, `type` has 4 distinct values, top covers 1 (< 3 floor).
        insert_doc(&cache, "a.md", r#"{"type":"note"}"#);
        insert_doc(&cache, "b.md", r#"{"type":"task"}"#);
        insert_doc(&cache, "c.md", r#"{"type":"log"}"#);
        insert_doc(&cache, "d.md", r#"{"type":"area"}"#);
        assert!(live_examples_for_find(&cache).is_empty());
    }

    #[test]
    fn single_predicate_when_no_second_field_qualifies() {
        let (_tmp, cache) = fresh_cache();
        // 3 docs all type=note; no other enum-like field.
        insert_doc(&cache, "a.md", r#"{"type":"note"}"#);
        insert_doc(&cache, "b.md", r#"{"type":"note"}"#);
        insert_doc(&cache, "c.md", r#"{"type":"note"}"#);
        let out = live_examples_for_find(&cache);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].query,
            format!(
                "{} find --eq type:note --limit 5",
                crate::help::bin_name::BIN_NAME
            )
        );
        assert_eq!(out[0].match_count, 3);
    }

    #[test]
    fn two_predicates_with_date_sort() {
        let (_tmp, cache) = fresh_cache();
        // 5 docs with type+workspace+modified — should compose all three.
        insert_doc(
            &cache,
            "a.md",
            r#"{"type":"note","workspace":"norn","modified":"2026-05-21"}"#,
        );
        insert_doc(
            &cache,
            "b.md",
            r#"{"type":"note","workspace":"norn","modified":"2026-05-20"}"#,
        );
        insert_doc(
            &cache,
            "c.md",
            r#"{"type":"note","workspace":"norn","modified":"2026-05-19"}"#,
        );
        insert_doc(
            &cache,
            "d.md",
            r#"{"type":"task","workspace":"norn","modified":"2026-05-18"}"#,
        );
        insert_doc(
            &cache,
            "e.md",
            r#"{"type":"task","workspace":"atlas","modified":"2026-05-17"}"#,
        );
        let out = live_examples_for_find(&cache);
        assert_eq!(out.len(), 1);
        let q = &out[0].query;
        assert!(q.contains("--eq type:note"), "got: {q}");
        assert!(q.contains("--eq workspace:norn"), "got: {q}");
        assert!(q.contains("--sort modified"), "got: {q}");
        assert!(q.ends_with("--limit 5"), "got: {q}");
        assert_eq!(out[0].match_count, 3);
    }

    #[test]
    fn date_like_preference_order_modified_beats_created() {
        let (_tmp, cache) = fresh_cache();
        insert_doc(
            &cache,
            "a.md",
            r#"{"type":"note","created":"2026-01-01","modified":"2026-05-21"}"#,
        );
        insert_doc(
            &cache,
            "b.md",
            r#"{"type":"note","created":"2026-01-01","modified":"2026-05-20"}"#,
        );
        insert_doc(
            &cache,
            "c.md",
            r#"{"type":"note","created":"2026-01-01","modified":"2026-05-19"}"#,
        );
        let out = live_examples_for_find(&cache);
        assert_eq!(out.len(), 1);
        assert!(out[0].query.contains("--sort modified"));
    }

    #[test]
    fn date_like_falls_back_to_created_when_modified_absent() {
        let (_tmp, cache) = fresh_cache();
        insert_doc(&cache, "a.md", r#"{"type":"note","created":"2026-01-01"}"#);
        insert_doc(&cache, "b.md", r#"{"type":"note","created":"2026-01-02"}"#);
        insert_doc(&cache, "c.md", r#"{"type":"note","created":"2026-01-03"}"#);
        let out = live_examples_for_find(&cache);
        assert_eq!(out.len(), 1);
        assert!(out[0].query.contains("--sort created"));
    }

    #[test]
    fn no_sort_when_no_date_like_field() {
        let (_tmp, cache) = fresh_cache();
        insert_doc(&cache, "a.md", r#"{"type":"note"}"#);
        insert_doc(&cache, "b.md", r#"{"type":"note"}"#);
        insert_doc(&cache, "c.md", r#"{"type":"note"}"#);
        let out = live_examples_for_find(&cache);
        assert_eq!(out.len(), 1);
        assert!(!out[0].query.contains("--sort"));
    }

    #[test]
    fn array_value_field_excluded_from_enum_like() {
        let (_tmp, cache) = fresh_cache();
        // 3 docs with array aliases and a `type` enum-like.
        insert_doc(&cache, "a.md", r#"{"type":"note","aliases":["x"]}"#);
        insert_doc(&cache, "b.md", r#"{"type":"note","aliases":["y"]}"#);
        insert_doc(&cache, "c.md", r#"{"type":"note","aliases":["z"]}"#);
        let out = live_examples_for_find(&cache);
        assert_eq!(out.len(), 1);
        // Query should NOT mention aliases.
        assert!(!out[0].query.contains("aliases"));
        assert!(out[0].query.contains("--eq type:note"));
    }

    #[test]
    fn enum_field_with_whitespace_in_top_value_excluded() {
        let (_tmp, cache) = fresh_cache();
        // 3 docs with status="in progress" — top value has whitespace,
        // so the field is dropped from the enum-like candidate set even
        // though it would otherwise qualify.
        insert_doc(&cache, "a.md", r#"{"status":"in progress"}"#);
        insert_doc(&cache, "b.md", r#"{"status":"in progress"}"#);
        insert_doc(&cache, "c.md", r#"{"status":"in progress"}"#);
        let out = live_examples_for_find(&cache);
        // No other enum-like field present → block omitted.
        assert!(out.is_empty());
    }

    #[test]
    fn wikilink_brackets_stripped_in_rendered_query() {
        let (_tmp, cache) = fresh_cache();
        // Atlas-shaped frontmatter — workspace stored as a wikilink. The
        // bracket-tolerant matcher accepts the bare form on input; the
        // rendered example must therefore strip the brackets so the user
        // can paste it without shell escaping.
        insert_doc(&cache, "a.md", r#"{"type":"note","workspace":"[[norn]]"}"#);
        insert_doc(&cache, "b.md", r#"{"type":"note","workspace":"[[norn]]"}"#);
        insert_doc(&cache, "c.md", r#"{"type":"note","workspace":"[[norn]]"}"#);
        let out = live_examples_for_find(&cache);
        assert_eq!(out.len(), 1);
        let q = &out[0].query;
        assert!(
            q.contains("--eq workspace:norn"),
            "expected bare value; got: {q}"
        );
        assert!(
            !q.contains("[[") && !q.contains("]]"),
            "wikilink brackets must be stripped; got: {q}"
        );
    }

    #[test]
    fn deterministic_across_calls() {
        let (_tmp, cache) = fresh_cache();
        insert_doc(&cache, "a.md", r#"{"type":"note","workspace":"alpha"}"#);
        insert_doc(&cache, "b.md", r#"{"type":"note","workspace":"alpha"}"#);
        insert_doc(&cache, "c.md", r#"{"type":"note","workspace":"alpha"}"#);
        insert_doc(&cache, "d.md", r#"{"type":"note","workspace":"beta"}"#);
        let a = live_examples_for_find(&cache);
        let b = live_examples_for_find(&cache);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].query, b[0].query);
        assert_eq!(a[0].match_count, b[0].match_count);
    }
}
