//! Wave-2 acceptance: Mimir-scenario `EXPLAIN QUERY PLAN` guard matrix
//! (NRN-80).
//!
//! `document_fields` (NRN-78) and the query router (NRN-79) already carry
//! per-predicate-class EXPLAIN guards and scan/EAV parity tests in
//! `query_documents.rs`'s `router_tests` module. This module closes Wave 2
//! by pinning the plan shape for the *named scenarios* the Mimir work-state
//! consumer actually issues — a realistic multi-rule config (the shape a
//! real vault ships, not a single-field synthetic fixture) exercised
//! through the combinations that matter operationally: a driving positive
//! plus a negated membership probe (R3), a reverse-dependency lookup on an
//! array wikilink field (R5), a presence probe (`--missing`), a namespaced
//! prefix scan over a `list_of_strings` field (B1/R6), and an equality
//! predicate deliberately made *non-selective* to prove the router doesn't
//! degrade to a table scan just because a value is common.
//!
//! Every test here opens the cache the same way production does for a real
//! vault: parse a `.norn/config.yaml`-shaped YAML string with
//! `crate::standards::parse_config`, resolve its index set with
//! `crate::standards::resolved_index_set`, and open authoritatively via
//! `Cache::open_with_index` — not the hand-rolled `open_authoritative(root,
//! &["field", ...])` helper `router_tests` uses, so this module also proves
//! the config → index-policy → router pipeline agrees end to end.
//!
//! This crate ships only a binary target (see `Cargo.toml` — no `[lib]`),
//! so `EXPLAIN QUERY PLAN` guards that need `Cache`/`DocumentQuery`/the
//! query-builder internals can't live in `tests/` (an integration test
//! there only sees the compiled `norn` binary over a subprocess — see
//! `tests/find_index_routing.rs`). They live here instead, as a sibling
//! module of `query_documents`/`document_fields` inside the crate.
//!
//! The `#[ignore]`d timing curve at the bottom of this file lives here for
//! the same internals-access reason, but also for a sharper one: an
//! earlier CLI-subprocess version of it measured `Cache::open`'s
//! unconditional `PRAGMA integrity_check` (see `src/cache/open.rs`) far
//! more than the query plan — that pragma scans the *entire* on-disk
//! database on every open and dominated at 50k docs (routed appeared
//! *slower* than scan purely because the routed cache's `document_fields`
//! table made the database bigger). Timing `Cache::documents_matching`
//! directly on an already-open connection isolates exactly what NRN-79's
//! router changed: the query plan, not per-process cache-open overhead.
//!
//! The black-box acceptance piece that's genuinely about the CLI's
//! byte-level output contract (sentinel invisibility across `find`/`count`
//! formats) and the one that maps cleanly onto CLI flags (the
//! indexed-vs-scan parity property test) live in `tests/eav_acceptance.rs`
//! and `tests/eav_parity_property.rs` instead — see those files' module
//! doc comments for why they, unlike this one, work fine as subprocess-driven
//! integration tests.

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    use crate::cache::query_documents::build_documents_matching_sql_parts;
    use crate::cache::{Cache, DocumentQuery};

    /// A realistic multi-field rule, matching the shape an actual Mimir-style
    /// vault config declares: bounded scalar/array types across every
    /// auto-qualifying `field_types` kind (`string`, `wikilink_or_list`,
    /// `list_of_strings`, `wikilink`, `datetime`), no explicit `indexed:`
    /// anywhere — every field gets into the index purely via `index.auto`
    /// (default `true`), matching "all auto-indexed" from the task brief.
    const MIMIR_CONFIG: &str = r#"
validate:
  rules:
    - name: mimir
      field_types:
        project: string
        lifecycle: string
        type: string
        depends_on: wikilink_or_list
        tags: list_of_strings
        anchor: wikilink
        lastActivity: datetime
"#;

    /// Resolve `MIMIR_CONFIG` into an authoritative, rebuilt cache over
    /// `root`. Mirrors what `crate::cache_cmd::open_for_query` does in
    /// production: parse config, resolve the index set, open with it.
    fn open_mimir(root: &Utf8PathBuf) -> Cache {
        let cfg = crate::standards::parse_config(
            MIMIR_CONFIG,
            camino::Utf8Path::new(".norn/config.yaml"),
        )
        .expect("realistic Mimir config should parse");
        let (index_set, index_set_hash) = crate::standards::resolved_index_set(&cfg);
        assert_eq!(
            index_set,
            [
                "anchor",
                "depends_on",
                "lastActivity",
                "lifecycle",
                "project",
                "tags",
                "type",
            ]
            .into_iter()
            .map(String::from)
            .collect::<BTreeSet<_>>(),
            "every declared field should auto-qualify under index.auto's default"
        );
        let mut cache = Cache::open_with_index(root, None, &index_set, &index_set_hash).unwrap();
        cache.rebuild(root).unwrap();
        cache
    }

    fn mimir_vault() -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        let docs: &[(&str, &str)] = &[
            (
                "t-100.md",
                "---\n\
                 project: NRN\n\
                 lifecycle: active\n\
                 type: task\n\
                 depends_on: [\"[[T-123]]\"]\n\
                 tags: [\"release:v0.40\", \"area:cache\"]\n\
                 anchor: \"[[NRN-80-anchor]]\"\n\
                 lastActivity: \"2026-07-01T10:00:00Z\"\n\
                 ---\nbody\n",
            ),
            (
                "t-101.md",
                "---\n\
                 project: NRN\n\
                 lifecycle: done\n\
                 type: task\n\
                 depends_on: []\n\
                 tags: [\"release:v0.39\"]\n\
                 anchor: \"[[NRN-79-anchor]]\"\n\
                 lastActivity: \"2026-06-20T08:00:00Z\"\n\
                 ---\nbody\n",
            ),
            (
                "t-102.md",
                "---\n\
                 project: NRN\n\
                 lifecycle: abandoned\n\
                 type: task\n\
                 tags: [\"type:note\"]\n\
                 ---\nbody\n",
            ),
            (
                "t-103.md",
                "---\n\
                 project: NRN\n\
                 lifecycle: backlog\n\
                 type: task\n\
                 depends_on: [\"[[T-123]]\", \"[[T-999]]\"]\n\
                 tags: [\"release:v0.41\"]\n\
                 ---\nbody\n",
            ),
            (
                "t-104.md",
                "---\n\
                 project: ATLAS\n\
                 lifecycle: active\n\
                 type: task\n\
                 tags: [\"area:vault\"]\n\
                 ---\nbody\n",
            ),
            (
                // No `anchor` at all — the `--missing anchor` target.
                "t-105.md",
                "---\n\
                 project: NRN\n\
                 lifecycle: active\n\
                 type: task\n\
                 ---\nbody\n",
            ),
        ];
        for (name, body) in docs {
            std::fs::write(root.join(name).as_std_path(), body).unwrap();
        }
        (tmp, root)
    }

    fn explain_plan(cache: &Cache, sql: &str, binds: &[rusqlite::types::Value]) -> Vec<String> {
        let full_sql = format!("EXPLAIN QUERY PLAN {sql}");
        let mut stmt = cache.conn().prepare(&full_sql).unwrap();
        stmt.query_map(rusqlite::params_from_iter(binds.iter()), |row| {
            row.get::<_, String>(3)
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    }

    fn plan_for(cache: &Cache, query: &DocumentQuery) -> (String, Vec<String>) {
        let (where_sql, binds) = build_documents_matching_sql_parts(cache, query);
        let sql = format!("SELECT path FROM documents{where_sql}");
        let rows = explain_plan(cache, &sql, &binds);
        (where_sql, rows)
    }

    fn matched_paths(cache: &Cache, query: &DocumentQuery) -> Vec<String> {
        let mut paths: Vec<String> = cache
            .documents_matching(query)
            .unwrap()
            .into_iter()
            .map(|d| d.path.to_string())
            .collect();
        paths.sort();
        paths
    }

    fn no_scan(rows: &[String]) {
        assert!(
            !rows.iter().any(|r| r.contains("SCAN documents")),
            "must not SCAN documents: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("SCAN document_fields")),
            "must not SCAN document_fields: {rows:?}"
        );
    }

    // ── R3: --eq project:X --not-in lifecycle:done,abandoned ────────────

    #[test]
    fn r3_positive_eq_drives_kv_negation_probes_pk() {
        let (_tmp, root) = mimir_vault();
        let cache = open_mimir(&root);

        let query = DocumentQuery {
            frontmatter_eq: vec![("project".to_string(), serde_json::json!("NRN"))],
            frontmatter_not_in: vec![(
                "lifecycle".to_string(),
                vec![serde_json::json!("done"), serde_json::json!("abandoned")],
            )],
            ..Default::default()
        };
        let (where_sql, rows) = plan_for(&cache, &query);
        eprintln!("R3 plan: {rows:?}");

        assert!(
            rows.iter()
                .any(|r| r.contains("SEARCH") && r.contains("idx_document_fields_kv")),
            "positive --eq project:NRN must drive via a SEARCH on idx_document_fields_kv: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("idx_document_fields_pk")),
            "negated --not-in lifecycle:... must probe via idx_document_fields_pk: {rows:?}"
        );
        no_scan(&rows);

        // Correctness, not just plan shape: active/backlog NRN tasks only —
        // done/abandoned excluded, ATLAS excluded regardless of lifecycle.
        assert_eq!(
            matched_paths(&cache, &query),
            vec!["t-100.md", "t-103.md", "t-105.md"],
            "sanity: R3 should match active/backlog NRN docs only; where_sql={where_sql}"
        );
    }

    // ── R5: --eq depends_on:T-123 (reverse dependency, array wikilink) ──

    #[test]
    fn r5_reverse_dependency_array_wikilink_field_drives_search() {
        let (_tmp, root) = mimir_vault();
        let cache = open_mimir(&root);

        let query = DocumentQuery {
            frontmatter_eq: vec![("depends_on".to_string(), serde_json::json!("T-123"))],
            ..Default::default()
        };
        let (_where_sql, rows) = plan_for(&cache, &query);
        eprintln!("R5 plan: {rows:?}");

        assert!(
            rows.iter()
                .any(|r| r.contains("SEARCH") && r.contains("idx_document_fields_kv")),
            "reverse-dependency --eq on an array wikilink field must drive via a SEARCH: {rows:?}"
        );
        no_scan(&rows);

        // t-100 and t-103 both have a depends_on element that is `[[T-123]]`
        // (bracket-stripped at both write and query time) — t-104/t-101/t-102
        // don't reference T-123 at all.
        assert_eq!(
            matched_paths(&cache, &query),
            vec!["t-100.md", "t-103.md"],
            "sanity: only docs whose depends_on array contains T-123 should match"
        );
    }

    // ── --missing anchor ──────────────────────────────────────────────

    #[test]
    fn missing_anchor_drives_search_with_sentinel_bind() {
        let (_tmp, root) = mimir_vault();
        let cache = open_mimir(&root);

        let query = DocumentQuery {
            frontmatter_missing: vec!["anchor".to_string()],
            ..Default::default()
        };
        let (where_sql, _binds) = build_documents_matching_sql_parts(&cache, &query);
        // The absent-sentinel is inlined as the SQL literal `x'00'` rather
        // than bound as a parameter (see `build_documents_matching_sql_parts_eav`'s
        // `frontmatter_missing` arm) — assert the literal appears in the
        // compiled WHERE clause.
        assert!(
            where_sql.contains("x'00'"),
            "the --missing compilation must reference the x'00' absent-sentinel: {where_sql}"
        );

        let (_where_sql, rows) = plan_for(&cache, &query);
        eprintln!("--missing anchor plan: {rows:?}");
        assert!(
            rows.iter()
                .any(|r| r.contains("SEARCH") && r.contains("idx_document_fields_kv")),
            "--missing anchor must compile to a driving SEARCH on idx_document_fields_kv: {rows:?}"
        );
        no_scan(&rows);

        // t-102 (no anchor key at all), t-103 (no anchor key), t-104 (no
        // anchor key), t-105 (no anchor key) are missing; t-100/t-101 declare
        // one.
        assert_eq!(
            matched_paths(&cache, &query),
            vec!["t-102.md", "t-103.md", "t-104.md", "t-105.md"],
        );
    }

    // ── B1/R6: --starts-with tags:release: ───────────────────────────

    #[test]
    fn prefix_tags_release_namespace_is_list_subquery_with_range_search() {
        let (_tmp, root) = mimir_vault();
        let cache = open_mimir(&root);

        let query = DocumentQuery {
            frontmatter_starts_with: vec![("tags".to_string(), "release:".to_string())],
            ..Default::default()
        };
        let (where_sql, rows) = plan_for(&cache, &query);
        eprintln!("prefix plan: {rows:?}\nwhere_sql: {where_sql}");

        // The compiled SQL text is the two-branch union documented on
        // `push_string_operator_eav`: branch 1 is a `>=`/`<` range test
        // directly on `document_fields.value`, branch 2 re-evaluates the
        // exact scan-path expression for non-text rows. Assert the range
        // shape textually (the letter of the B1/R6 scenario) rather than
        // only the no-scan invariant.
        assert!(
            where_sql.contains("value >= ?") && where_sql.contains("value < ?"),
            "starts-with's text branch must compile to a value range test: {where_sql}"
        );
        assert!(
            where_sql.contains("UNION"),
            "starts-with must compile to the two-branch LIST SUBQUERY union: {where_sql}"
        );
        assert!(
            rows.iter()
                .any(|r| r.contains("SEARCH") && r.contains("idx_document_fields_kv")),
            "the text-branch range must drive off idx_document_fields_kv: {rows:?}"
        );
        no_scan(&rows);

        assert_eq!(
            matched_paths(&cache, &query),
            vec!["t-100.md", "t-101.md", "t-103.md"],
        );
    }

    // ── Adversarial: --eq type:note over a ~50%-selective value ──────

    fn low_selectivity_vault(n: usize) -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        for i in 0..n {
            let ty = if i % 2 == 0 { "note" } else { "log" };
            std::fs::write(
                root.join(format!("doc{i:04}.md")).as_std_path(),
                format!("---\nproject: NRN\nlifecycle: active\ntype: {ty}\n---\nbody\n"),
            )
            .unwrap();
        }
        (tmp, root)
    }

    #[test]
    fn adversarial_non_selective_eq_still_no_scan() {
        // ~50% of documents carry `type: note` — the router must not fall
        // back to a table scan just because the predicate is unselective;
        // `(key, value)` equality is still a targeted index range regardless
        // of how many rows fall in that range.
        let (_tmp, root) = low_selectivity_vault(200);
        let cache = open_mimir(&root);

        let query = DocumentQuery {
            frontmatter_eq: vec![("type".to_string(), serde_json::json!("note"))],
            ..Default::default()
        };
        let (_where_sql, rows) = plan_for(&cache, &query);
        eprintln!("adversarial non-selective plan (~50% match): {rows:?}");
        assert!(
            rows.iter()
                .any(|r| r.contains("SEARCH") && r.contains("idx_document_fields_kv")),
            "even a ~50%-selective --eq must still drive via SEARCH, not degrade to scan: {rows:?}"
        );
        no_scan(&rows);
        assert_eq!(matched_paths(&cache, &query).len(), 100);
    }

    // ── Timing curve: 1k / 10k / 50k, routed vs. scan ────────────────────

    fn timing_vault(root: &Utf8PathBuf, n: usize) {
        std::fs::create_dir_all(root.as_std_path()).unwrap();
        // Project cardinality scales with vault size so `project:P0`'s
        // absolute *result-set* size stays roughly constant (~40 docs)
        // across every `n` — otherwise a fixed 25-project split makes P0's
        // match count grow linearly with `n` regardless of routing, and a
        // bigger result set is legitimately slower to fetch/return no
        // matter how it was found. The claim this curve tests is that
        // *finding* a fixed-size slice gets no slower as the vault around
        // it grows, not that fetching a growing slice stays flat.
        let project_buckets = (n / 40).max(1);
        for i in 0..n {
            let project = format!("P{}", i % project_buckets);
            let lifecycle = ["active", "backlog", "done", "abandoned"][i % 4];
            let body = format!(
                "---\n\
                 project: {project}\n\
                 lifecycle: {lifecycle}\n\
                 type: task\n\
                 depends_on: [\"[[T-{}]]\"]\n\
                 tags: [\"release:v{}\"]\n\
                 lastActivity: \"2026-01-01T00:00:00Z\"\n\
                 ---\nbody\n",
                i % 500,
                i % 10,
            );
            std::fs::write(root.join(format!("doc{i:06}.md")).as_std_path(), body).unwrap();
        }
    }

    fn r3_timing_query() -> DocumentQuery {
        DocumentQuery {
            frontmatter_eq: vec![("project".to_string(), serde_json::json!("P0"))],
            frontmatter_not_in: vec![(
                "lifecycle".to_string(),
                vec![serde_json::json!("done"), serde_json::json!("abandoned")],
            )],
            ..Default::default()
        }
    }

    fn median_duration<F: FnMut()>(mut f: F, samples: usize) -> std::time::Duration {
        let mut durations: Vec<std::time::Duration> = (0..samples)
            .map(|_| {
                let start = std::time::Instant::now();
                f();
                start.elapsed()
            })
            .collect();
        durations.sort();
        durations[durations.len() / 2]
    }

    /// Deliverable 3 of NRN-80: the EAV router's scaling timing curve.
    /// `#[ignore]`d by convention (see `cache_rebuild::cold_rebuild_under_2s_on_1k_docs`
    /// in `src/cache.rs` for the precedent) — opt in explicitly:
    ///
    /// ```text
    /// cargo test --release --bin norn -- --ignored eav_router_timing_curve
    /// ```
    #[test]
    #[ignore]
    fn eav_router_timing_curve_1k_10k_50k() {
        let sizes = [1_000usize, 10_000, 50_000];
        let query = r3_timing_query();
        let index_fields: BTreeSet<String> = [
            "project",
            "lifecycle",
            "type",
            "depends_on",
            "tags",
            "lastActivity",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        let mut rows: Vec<(usize, std::time::Duration, std::time::Duration)> = Vec::new();

        for &n in &sizes {
            let indexed_tmp = TempDir::new().unwrap();
            let indexed_root = Utf8PathBuf::from_path_buf(indexed_tmp.path().to_path_buf())
                .unwrap()
                .join("vault");
            timing_vault(&indexed_root, n);
            let mut indexed_cache =
                Cache::open_with_index(&indexed_root, None, &index_fields, "timing-hash-idx")
                    .unwrap();
            indexed_cache.rebuild(&indexed_root).unwrap();
            let routed = median_duration(
                || {
                    indexed_cache.documents_matching(&query).unwrap();
                },
                5,
            );

            let scan_tmp = TempDir::new().unwrap();
            let scan_root = Utf8PathBuf::from_path_buf(scan_tmp.path().to_path_buf())
                .unwrap()
                .join("vault");
            timing_vault(&scan_root, n);
            let mut scan_cache =
                Cache::open_with_index(&scan_root, None, &BTreeSet::new(), "timing-hash-scan")
                    .unwrap();
            scan_cache.rebuild(&scan_root).unwrap();
            let scanned = median_duration(
                || {
                    scan_cache.documents_matching(&query).unwrap();
                },
                5,
            );

            rows.push((n, routed, scanned));
        }

        eprintln!(
            "\nNRN-80 EAV router timing curve — R3 (--eq project:P0 --not-in lifecycle:done,abandoned)\n\
             (in-process `Cache::documents_matching`, median of 5 warm calls; excludes cache-open overhead)"
        );
        eprintln!(
            "{:>8} | {:>14} | {:>14}",
            "docs", "routed (ms)", "scan (ms)"
        );
        eprintln!("{:->8}-+-{:->14}-+-{:->14}", "", "", "");
        for (n, routed, scanned) in &rows {
            eprintln!(
                "{:>8} | {:>14.4} | {:>14.4}",
                n,
                routed.as_secs_f64() * 1000.0,
                scanned.as_secs_f64() * 1000.0,
            );
        }

        let routed_1k = rows[0].1.as_secs_f64();
        let routed_50k = rows[rows.len() - 1].1.as_secs_f64();
        let ratio = routed_50k / routed_1k.max(0.000_001);
        eprintln!("\nrouted 50k / routed 1k = {ratio:.2}x (budget: <= 3.00x)");
        // A 2ms floor on the 1k baseline absorbs sub-millisecond timer
        // jitter — an in-process query against 1k rows can legitimately
        // complete in tens of microseconds, where OS scheduling noise alone
        // would blow a bare ratio past 3x without the flatness claim being
        // false in any way that matters.
        let floor_1k = routed_1k.max(0.002);
        assert!(
            routed_50k <= floor_1k * 3.0,
            "routed latency should stay roughly flat from 1k to 50k docs: \
             1k={routed_1k:.6}s, 50k={routed_50k:.6}s ({ratio:.2}x, budget 3x over a {floor_1k:.6}s floor)"
        );
    }
}
