//! Round-trip property tests for `Cache::find_documents`: verify
//! ORDER BY / LIMIT / OFFSET / COUNT semantics across realistic vaults.

use camino::Utf8PathBuf;
use tempfile::TempDir;
use vault_cache::{Cache, DocumentQuery, FindQuery, FindResult, SortClause, SortDirection};

fn synth_paged_vault() -> (TempDir, Utf8PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
        .unwrap()
        .join("vault");
    std::fs::create_dir(root.as_std_path()).unwrap();
    // 25 docs: doc-01.md through doc-25.md, each with `priority: <N>`.
    for i in 1..=25 {
        let priority = (i * 7) % 100;
        std::fs::write(
            root.join(format!("doc-{:02}.md", i)).as_std_path(),
            format!("---\ntype: note\npriority: {}\n---\nbody-{}\n", priority, i),
        )
        .unwrap();
    }
    (tmp, root)
}

fn paths(result: &FindResult) -> Vec<&str> {
    result.matches.iter().map(|d| d.path.as_str()).collect()
}

#[test]
fn empty_query_default_paging() {
    let (_tmp, root) = synth_paged_vault();
    let mut cache = Cache::open(&root).unwrap();
    cache.rebuild(&root).unwrap();

    let q = FindQuery {
        starts_at: 1,
        ..Default::default()
    };
    let result = cache.find_documents(&q).unwrap();

    assert_eq!(result.total, 25);
    assert_eq!(result.returned, 25);
    assert!(!result.truncated);
    assert_eq!(result.matches[0].path.as_str(), "doc-01.md");
    assert_eq!(result.matches[24].path.as_str(), "doc-25.md");
}

#[test]
fn limit_truncates_signals_total() {
    let (_tmp, root) = synth_paged_vault();
    let mut cache = Cache::open(&root).unwrap();
    cache.rebuild(&root).unwrap();

    let q = FindQuery {
        starts_at: 1,
        limit: Some(10),
        ..Default::default()
    };
    let result = cache.find_documents(&q).unwrap();

    assert_eq!(result.total, 25);
    assert_eq!(result.returned, 10);
    assert!(result.truncated);
    assert_eq!(paths(&result)[0], "doc-01.md");
    assert_eq!(paths(&result)[9], "doc-10.md");
}

#[test]
fn paging_starts_at_returns_window() {
    let (_tmp, root) = synth_paged_vault();
    let mut cache = Cache::open(&root).unwrap();
    cache.rebuild(&root).unwrap();

    let q = FindQuery {
        starts_at: 11,
        limit: Some(10),
        ..Default::default()
    };
    let result = cache.find_documents(&q).unwrap();

    assert_eq!(result.total, 25);
    assert_eq!(result.returned, 10);
    assert_eq!(paths(&result)[0], "doc-11.md");
    assert_eq!(paths(&result)[9], "doc-20.md");
}

#[test]
fn paging_beyond_end_returns_empty() {
    let (_tmp, root) = synth_paged_vault();
    let mut cache = Cache::open(&root).unwrap();
    cache.rebuild(&root).unwrap();

    let q = FindQuery {
        starts_at: 100,
        limit: Some(10),
        ..Default::default()
    };
    let result = cache.find_documents(&q).unwrap();

    assert_eq!(result.total, 25);
    assert_eq!(result.returned, 0);
    assert!(result.truncated); // 0 returned but 25 total → truncated
}

#[test]
fn sort_by_frontmatter_field_asc() {
    let (_tmp, root) = synth_paged_vault();
    let mut cache = Cache::open(&root).unwrap();
    cache.rebuild(&root).unwrap();

    let q = FindQuery {
        starts_at: 1,
        limit: Some(3),
        sort: Some(SortClause {
            field: "priority".to_string(),
            direction: SortDirection::Asc,
        }),
        ..Default::default()
    };
    let result = cache.find_documents(&q).unwrap();

    let prios: Vec<i64> = result
        .matches
        .iter()
        .map(|d| {
            d.frontmatter.as_ref().unwrap()["priority"]
                .as_i64()
                .unwrap()
        })
        .collect();
    let mut sorted_prios = prios.clone();
    sorted_prios.sort();
    assert_eq!(prios, sorted_prios);
}

#[test]
fn sort_by_path_desc() {
    let (_tmp, root) = synth_paged_vault();
    let mut cache = Cache::open(&root).unwrap();
    cache.rebuild(&root).unwrap();

    let q = FindQuery {
        starts_at: 1,
        limit: Some(3),
        sort: Some(SortClause {
            field: "path".to_string(),
            direction: SortDirection::Desc,
        }),
        ..Default::default()
    };
    let result = cache.find_documents(&q).unwrap();

    assert_eq!(paths(&result), vec!["doc-25.md", "doc-24.md", "doc-23.md"]);
}

#[test]
fn predicate_narrows_then_sort_and_limit() {
    let (_tmp, root) = synth_paged_vault();
    let mut cache = Cache::open(&root).unwrap();
    cache.rebuild(&root).unwrap();

    let q = FindQuery {
        predicates: DocumentQuery {
            frontmatter_eq: vec![("type".to_string(), serde_json::json!("note"))],
            ..Default::default()
        },
        starts_at: 1,
        limit: Some(5),
        sort: Some(SortClause {
            field: "path".to_string(),
            direction: SortDirection::Asc,
        }),
    };
    let result = cache.find_documents(&q).unwrap();

    assert_eq!(result.total, 25);
    assert_eq!(result.returned, 5);
    assert!(result.truncated);
    assert_eq!(paths(&result)[0], "doc-01.md");
    assert_eq!(paths(&result)[4], "doc-05.md");
}

#[test]
fn path_glob_post_pass_affects_total_and_matches() {
    let (_tmp, root) = synth_paged_vault();
    let mut cache = Cache::open(&root).unwrap();
    cache.rebuild(&root).unwrap();

    // Glob matches docs ending in 0 (doc-10.md, doc-20.md). doc-01..09 don't.
    let q = FindQuery {
        predicates: DocumentQuery {
            path_globs: vec!["doc-*0.md".to_string()],
            ..Default::default()
        },
        starts_at: 1,
        limit: Some(100),
        ..Default::default()
    };
    let result = cache.find_documents(&q).unwrap();

    assert_eq!(result.total, 2);
    assert_eq!(result.returned, 2);
    assert!(!result.truncated);
}

#[test]
fn find_documents_query_plan_is_single_select() {
    let (_tmp, root) = synth_paged_vault();
    let mut cache = Cache::open(&root).unwrap();
    cache.rebuild(&root).unwrap();

    let conn = cache.conn_for_test();
    let mut stmt = conn
        .prepare(
            "EXPLAIN QUERY PLAN \
         SELECT path, stem, hash, frontmatter_json, body_text \
         FROM documents WHERE json_extract(frontmatter_json, ?) = ? \
         ORDER BY json_extract(frontmatter_json, ?) ASC, path ASC \
         LIMIT ? OFFSET ?",
        )
        .unwrap();
    let rows: Vec<String> = stmt
        .query_map(
            rusqlite::params![r#"$."type""#, "note", r#"$."priority""#, 10i64, 0i64],
            |row| row.get::<_, String>(3),
        )
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert!(
        !rows.iter().any(|r| r.contains("SUBQUERY")),
        "find_documents plan contains a subquery: {:?}",
        rows
    );
    assert!(
        rows.iter().any(|r| r.contains("documents")),
        "plan does not mention documents table: {:?}",
        rows
    );
}
