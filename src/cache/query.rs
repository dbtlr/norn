//! Predicate set for `Cache::documents_matching` and the JSON-path encoder
//! that translates frontmatter field names into safe SQLite JSON paths.

use serde_json::Value;

/// Predicate set for `Cache::documents_matching` and `Cache::find_documents`.
/// ANY-of within `path_globs` and within each `frontmatter_in` value list;
/// ALL-of across all flag-fields and across vectors.
#[derive(Default, Debug, Clone)]
pub struct DocumentQuery {
    /// Path glob patterns in `crate::standards::path_match::PathPattern` syntax.
    /// ANY-of. Empty = no path narrowing. Applied as a Rust post-pass.
    pub path_globs: Vec<String>,
    /// Frontmatter equality predicates `(field, value)`. ALL-of.
    pub frontmatter_eq: Vec<(String, Value)>,
    /// Frontmatter inequality predicates `(field, value)` — negation of
    /// `frontmatter_eq`. For array-shaped string fields, matches when no
    /// element equals the value. ALL-of.
    pub frontmatter_not_eq: Vec<(String, Value)>,
    /// Required-present fields. ALL-of. Match v1 filter_documents semantics
    /// for null-vs-missing — verified via round-trip property tests.
    pub frontmatter_has: Vec<String>,
    /// Required-absent fields. ALL-of.
    pub frontmatter_missing: Vec<String>,
    /// `(field, allowed_values)` — frontmatter field is one of the values
    /// (ANY-of within each entry; ALL-of across entries).
    pub frontmatter_in: Vec<(String, Vec<Value>)>,
    /// `(field, disallowed_values)` — frontmatter field is NOT one of the values.
    pub frontmatter_not_in: Vec<(String, Vec<Value>)>,
    /// `(field, needle)` — `field` starts with `needle`. ALL-of. Anchored
    /// string operator: case-sensitive, array-aware (any element may match),
    /// wikilink-bracket-collapsed on both sides like `frontmatter_eq`.
    /// Non-string stored values coerce to their SQLite text rendering. A
    /// needle that is empty after bracket-stripping matches nothing.
    pub frontmatter_starts_with: Vec<(String, String)>,
    /// `(field, needle)` — `field` ends with `needle`. Same semantics as
    /// `frontmatter_starts_with`.
    pub frontmatter_ends_with: Vec<(String, String)>,
    /// `(field, needle)` — `field` contains `needle` as a substring. Same
    /// semantics as `frontmatter_starts_with`.
    pub frontmatter_contains: Vec<(String, String)>,
    /// `(field, date_string)` — `field` < `date_string` (lexical, ISO 8601).
    pub date_before: Vec<(String, String)>,
    /// `(field, date_string)` — `field` > `date_string`.
    pub date_after: Vec<(String, String)>,
    /// `(field, date_string)` — `field` = `date_string`.
    pub date_on: Vec<(String, String)>,
    /// Body-text substring; case-insensitive. v1: SQL LIKE. v4: FTS5.
    pub body_text_contains: Option<String>,
    /// Documents whose outgoing links resolve to ALL of these (resolved) paths.
    /// ALL-of. Resolved-only: matched against `links.resolved_path`. Targets are
    /// resolved to paths at the command layer (see `filter_args::resolve_links_to`).
    pub links_to: Vec<camino::Utf8PathBuf>,
    /// True ⇒ restrict to documents with ≥1 link whose `status = 'unresolved'`.
    /// Ambiguous-status links are excluded (distinct state, own validate codes).
    pub has_unresolved_links: bool,
}

/// Encode a frontmatter field name as a single quoted JSON-path segment for
/// SQLite's `json_extract`. Returns the full path string `$."<escaped>"`.
///
/// SQLite parses the path at statement execution; binding this as a parameter
/// (not interpolating it) is what closes the SQL-injection vector and lets
/// frontmatter keys contain any character.
pub fn json_path_for(field: &str) -> String {
    let escaped = field.replace('\\', r"\\").replace('"', r#"\""#);
    format!(r#"$."{}""#, escaped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_field() {
        assert_eq!(json_path_for("type"), r#"$."type""#);
    }

    #[test]
    fn hyphenated_field() {
        assert_eq!(json_path_for("created-at"), r#"$."created-at""#);
    }

    #[test]
    fn dotted_field() {
        // Keys with dots are flat keys (single quoted segment), not nested paths.
        assert_eq!(json_path_for("schema.version"), r#"$."schema.version""#);
    }

    #[test]
    fn embedded_quote_is_escaped() {
        assert_eq!(json_path_for(r#"a"b"#), r#"$."a\"b""#);
    }

    #[test]
    fn embedded_backslash_is_escaped() {
        assert_eq!(json_path_for(r"a\b"), r#"$."a\\b""#);
    }

    /// Round-trip property tests: cache-direct query results must match the
    /// equivalent `filter_documents` results against a `load_graph_index()` graph.
    mod property {
        use camino::Utf8PathBuf;
        use tempfile::TempDir;

        use crate::cache::{Cache, DocumentQuery};
        use crate::core::DocumentSummary;

        fn synth_vault() -> (TempDir, Utf8PathBuf) {
            let tmp = TempDir::new().unwrap();
            let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
                .unwrap()
                .join("vault");
            std::fs::create_dir(root.as_std_path()).unwrap();
            std::fs::write(
                root.join("note-a.md").as_std_path(),
                "---\ntype: note\nkind: log\n---\nbody a\n",
            )
            .unwrap();
            std::fs::write(
                root.join("note-b.md").as_std_path(),
                "---\ntype: note\nkind: meeting\n---\nbody b\n",
            )
            .unwrap();
            std::fs::write(
                root.join("workspace.md").as_std_path(),
                "---\ntype: workspace\n---\nbody w\n",
            )
            .unwrap();
            std::fs::write(
                root.join("untyped.md").as_std_path(),
                "no frontmatter at all\n",
            )
            .unwrap();
            (tmp, root)
        }

        fn populate_cache(root: &Utf8PathBuf) -> Cache {
            let mut cache = Cache::open(root).unwrap();
            cache.rebuild(root).unwrap();
            cache
        }

        fn paths(docs: &[DocumentSummary]) -> Vec<&str> {
            docs.iter().map(|d| d.path.as_str()).collect()
        }

        #[test]
        fn empty_query_returns_every_document_in_path_order() {
            let (_tmp, root) = synth_vault();
            let cache = populate_cache(&root);

            let result = cache.documents_matching(&DocumentQuery::default()).unwrap();

            assert_eq!(
                paths(&result),
                vec!["note-a.md", "note-b.md", "untyped.md", "workspace.md"]
            );
        }

        fn synth_vault_wikilink_shapes() -> (TempDir, Utf8PathBuf) {
            let tmp = TempDir::new().unwrap();
            let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
                .unwrap()
                .join("vault");
            std::fs::create_dir(root.as_std_path()).unwrap();
            std::fs::write(
                root.join("scalar-wikilink.md").as_std_path(),
                "---\nworkspace: \"[[norn]]\"\n---\nbody\n",
            )
            .unwrap();
            std::fs::write(
                root.join("scalar-plain.md").as_std_path(),
                "---\nworkspace: norn\n---\nbody\n",
            )
            .unwrap();
            std::fs::write(
                root.join("array-wikilinks.md").as_std_path(),
                "---\nsource_notes:\n  - \"[[seed-note]]\"\n  - \"[[other-note]]\"\n---\nbody\n",
            )
            .unwrap();
            std::fs::write(
                root.join("array-plain.md").as_std_path(),
                "---\ntags:\n  - foo\n  - bar\n---\nbody\n",
            )
            .unwrap();
            (tmp, root)
        }

        #[test]
        fn frontmatter_eq_string_matches_scalar_wikilink_without_brackets() {
            let (_tmp, root) = synth_vault_wikilink_shapes();
            let cache = populate_cache(&root);
            let query = DocumentQuery {
                frontmatter_eq: vec![("workspace".to_string(), serde_json::json!("norn"))],
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();
            let p = paths(&result);
            assert!(p.contains(&"scalar-wikilink.md"));
            assert!(p.contains(&"scalar-plain.md"));
        }

        #[test]
        fn frontmatter_eq_string_matches_array_element_without_brackets() {
            let (_tmp, root) = synth_vault_wikilink_shapes();
            let cache = populate_cache(&root);
            let query = DocumentQuery {
                frontmatter_eq: vec![("source_notes".to_string(), serde_json::json!("seed-note"))],
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();
            assert_eq!(paths(&result), vec!["array-wikilinks.md"]);
        }

        #[test]
        fn frontmatter_eq_string_with_explicit_brackets_still_matches() {
            let (_tmp, root) = synth_vault_wikilink_shapes();
            let cache = populate_cache(&root);
            let query = DocumentQuery {
                frontmatter_eq: vec![(
                    "source_notes".to_string(),
                    serde_json::json!("[[seed-note]]"),
                )],
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();
            assert_eq!(paths(&result), vec!["array-wikilinks.md"]);
        }

        #[test]
        fn frontmatter_eq_string_matches_array_of_plain_strings() {
            let (_tmp, root) = synth_vault_wikilink_shapes();
            let cache = populate_cache(&root);
            let query = DocumentQuery {
                frontmatter_eq: vec![("tags".to_string(), serde_json::json!("foo"))],
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();
            assert_eq!(paths(&result), vec!["array-plain.md"]);
        }

        #[test]
        fn frontmatter_not_eq_string_excludes_matching_scalar() {
            let (_tmp, root) = synth_vault_wikilink_shapes();
            let cache = populate_cache(&root);
            let query = DocumentQuery {
                frontmatter_has: vec!["workspace".to_string()],
                frontmatter_not_eq: vec![("workspace".to_string(), serde_json::json!("norn"))],
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();
            assert!(
                result.is_empty(),
                "both workspace docs match 'norn' (scalar+wikilink); --not-eq should exclude both: {result:?}"
            );
        }

        #[test]
        fn frontmatter_not_eq_string_excludes_array_match() {
            let (_tmp, root) = synth_vault_wikilink_shapes();
            let cache = populate_cache(&root);
            let query = DocumentQuery {
                frontmatter_has: vec!["source_notes".to_string()],
                frontmatter_not_eq: vec![(
                    "source_notes".to_string(),
                    serde_json::json!("seed-note"),
                )],
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();
            assert!(
                result.is_empty(),
                "array-wikilinks contains seed-note; --not-eq should exclude: {result:?}"
            );
        }

        #[test]
        fn frontmatter_in_string_matches_scalar_wikilink_without_brackets() {
            let (_tmp, root) = synth_vault_wikilink_shapes();
            let cache = populate_cache(&root);
            let query = DocumentQuery {
                frontmatter_in: vec![(
                    "workspace".to_string(),
                    vec![serde_json::json!("norn"), serde_json::json!("atlas")],
                )],
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();
            let p = paths(&result);
            assert!(p.contains(&"scalar-wikilink.md"));
            assert!(p.contains(&"scalar-plain.md"));
        }

        #[test]
        fn frontmatter_in_string_matches_array_element_without_brackets() {
            let (_tmp, root) = synth_vault_wikilink_shapes();
            let cache = populate_cache(&root);
            let query = DocumentQuery {
                frontmatter_in: vec![(
                    "source_notes".to_string(),
                    vec![
                        serde_json::json!("seed-note"),
                        serde_json::json!("missing-note"),
                    ],
                )],
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();
            assert_eq!(paths(&result), vec!["array-wikilinks.md"]);
        }

        #[test]
        fn frontmatter_not_in_string_excludes_array_match_and_keeps_others() {
            let (_tmp, root) = synth_vault_wikilink_shapes();
            let cache = populate_cache(&root);
            // Restrict to docs that HAVE source_notes, then exclude those whose
            // array contains "seed-note".
            let query = DocumentQuery {
                frontmatter_has: vec!["source_notes".to_string()],
                frontmatter_not_in: vec![(
                    "source_notes".to_string(),
                    vec![serde_json::json!("seed-note")],
                )],
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();
            assert!(
                result.is_empty(),
                "expected array-wikilinks excluded: {result:?}"
            );
        }

        #[test]
        fn string_operator_bracket_only_needle_matches_nothing() {
            let (_tmp, root) = synth_vault_wikilink_shapes();
            let cache = populate_cache(&root);
            // "[[]]" strips to the empty needle, which has no meaningful
            // anchored-match semantics — deterministically match nothing.
            let query = DocumentQuery {
                frontmatter_starts_with: vec![("workspace".to_string(), "[[]]".to_string())],
                ..Default::default()
            };
            assert!(cache.documents_matching(&query).unwrap().is_empty());
        }

        #[test]
        fn string_operator_coerces_non_string_scalar_to_text() {
            let tmp = TempDir::new().unwrap();
            let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
                .unwrap()
                .join("vault");
            std::fs::create_dir(root.as_std_path()).unwrap();
            std::fs::write(
                root.join("num.md").as_std_path(),
                "---\npriority: 123\n---\n",
            )
            .unwrap();
            let mut cache = Cache::open(&root).unwrap();
            cache.rebuild(&root).unwrap();

            // Documented text-coercion contract: 123 renders as "123".
            let query = DocumentQuery {
                frontmatter_contains: vec![("priority".to_string(), "2".to_string())],
                ..Default::default()
            };
            assert_eq!(
                paths(&cache.documents_matching(&query).unwrap()),
                vec!["num.md"]
            );

            let query = DocumentQuery {
                frontmatter_starts_with: vec![("priority".to_string(), "12".to_string())],
                ..Default::default()
            };
            assert_eq!(
                paths(&cache.documents_matching(&query).unwrap()),
                vec!["num.md"]
            );
        }

        #[test]
        fn string_operator_matches_boolean_source_text() {
            let tmp = TempDir::new().unwrap();
            let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
                .unwrap()
                .join("vault");
            std::fs::create_dir(root.as_std_path()).unwrap();
            std::fs::write(
                root.join("bool.md").as_std_path(),
                "---\narchived: true\nflags:\n  - false\n---\n",
            )
            .unwrap();
            let mut cache = Cache::open(&root).unwrap();
            cache.rebuild(&root).unwrap();

            // Booleans compare as their source text `true`/`false`, not
            // SQLite's 1/0 extraction.
            let query = DocumentQuery {
                frontmatter_contains: vec![("archived".to_string(), "true".to_string())],
                ..Default::default()
            };
            assert_eq!(
                paths(&cache.documents_matching(&query).unwrap()),
                vec!["bool.md"]
            );

            let query = DocumentQuery {
                frontmatter_contains: vec![("archived".to_string(), "1".to_string())],
                ..Default::default()
            };
            assert!(
                cache.documents_matching(&query).unwrap().is_empty(),
                "the SQLite integer rendering must not leak into matching"
            );

            // Array elements get the same treatment.
            let query = DocumentQuery {
                frontmatter_starts_with: vec![("flags".to_string(), "fal".to_string())],
                ..Default::default()
            };
            assert_eq!(
                paths(&cache.documents_matching(&query).unwrap()),
                vec!["bool.md"]
            );
        }

        #[test]
        fn string_operator_missing_field_never_matches() {
            let (_tmp, root) = synth_vault();
            let cache = populate_cache(&root);
            let query = DocumentQuery {
                frontmatter_contains: vec![("nonexistent".to_string(), "x".to_string())],
                ..Default::default()
            };
            assert!(cache.documents_matching(&query).unwrap().is_empty());
        }

        #[test]
        fn frontmatter_eq_string_value() {
            let (_tmp, root) = synth_vault();
            let cache = populate_cache(&root);

            let query = DocumentQuery {
                frontmatter_eq: vec![("type".to_string(), serde_json::json!("note"))],
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();

            assert_eq!(paths(&result), vec!["note-a.md", "note-b.md"]);
        }

        #[test]
        fn frontmatter_eq_multiple_fields_all_must_match() {
            let (_tmp, root) = synth_vault();
            let cache = populate_cache(&root);

            let query = DocumentQuery {
                frontmatter_eq: vec![
                    ("type".to_string(), serde_json::json!("note")),
                    ("kind".to_string(), serde_json::json!("log")),
                ],
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();

            assert_eq!(paths(&result), vec!["note-a.md"]);
        }

        #[test]
        fn frontmatter_has_present_field() {
            let (_tmp, root) = synth_vault();
            let cache = populate_cache(&root);

            let query = DocumentQuery {
                frontmatter_has: vec!["kind".to_string()],
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();

            assert_eq!(paths(&result), vec!["note-a.md", "note-b.md"]);
        }

        #[test]
        fn frontmatter_missing_absent_field() {
            let (_tmp, root) = synth_vault();
            let cache = populate_cache(&root);

            let query = DocumentQuery {
                frontmatter_missing: vec!["kind".to_string()],
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();

            assert_eq!(paths(&result), vec!["untyped.md", "workspace.md"]);
        }

        #[test]
        fn path_globs_post_filter() {
            let (_tmp, root) = synth_vault();
            let cache = populate_cache(&root);

            let query = DocumentQuery {
                path_globs: vec!["note-*.md".to_string()],
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();

            assert_eq!(paths(&result), vec!["note-a.md", "note-b.md"]);
        }

        #[test]
        fn path_globs_combined_with_frontmatter() {
            let (_tmp, root) = synth_vault();
            let cache = populate_cache(&root);

            let query = DocumentQuery {
                path_globs: vec!["note-*.md".to_string()],
                frontmatter_eq: vec![("kind".to_string(), serde_json::json!("meeting"))],
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();

            assert_eq!(paths(&result), vec!["note-b.md"]);
        }

        #[test]
        fn hyphenated_and_dotted_frontmatter_keys() {
            let tmp = TempDir::new().unwrap();
            let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
                .unwrap()
                .join("vault");
            std::fs::create_dir(root.as_std_path()).unwrap();
            std::fs::write(
                root.join("hyph.md").as_std_path(),
                "---\ncreated-at: 2026-01-01\n---\n",
            )
            .unwrap();
            std::fs::write(
                root.join("dotted.md").as_std_path(),
                "---\nschema.version: 3\n---\n",
            )
            .unwrap();
            let mut cache = Cache::open(&root).unwrap();
            cache.rebuild(&root).unwrap();

            let query = DocumentQuery {
                frontmatter_has: vec!["created-at".to_string()],
                ..Default::default()
            };
            assert_eq!(
                paths(&cache.documents_matching(&query).unwrap()),
                vec!["hyph.md"]
            );

            let query = DocumentQuery {
                frontmatter_has: vec!["schema.version".to_string()],
                ..Default::default()
            };
            assert_eq!(
                paths(&cache.documents_matching(&query).unwrap()),
                vec!["dotted.md"]
            );
        }

        #[test]
        fn document_by_path_returns_full_document() {
            let tmp = TempDir::new().unwrap();
            let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
                .unwrap()
                .join("vault");
            std::fs::create_dir(root.as_std_path()).unwrap();
            std::fs::write(
                root.join("doc.md").as_std_path(),
                "---\ntype: note\n---\n\n# Heading\n\n^block-1\n\n[link](other.md)\n",
            )
            .unwrap();
            std::fs::write(
                root.join("other.md").as_std_path(),
                "---\ntype: note\n---\nbody\n",
            )
            .unwrap();
            let mut cache = Cache::open(&root).unwrap();
            cache.rebuild(&root).unwrap();

            let doc = cache
                .document_by_path(camino::Utf8Path::new("doc.md"))
                .unwrap();

            let doc = doc.expect("doc.md should be present");
            assert_eq!(doc.path.as_str(), "doc.md");
            assert!(doc.headings.iter().any(|h| h.text == "Heading"));
            assert!(doc.block_ids.iter().any(|b| b == "block-1"));
            assert_eq!(doc.links.len(), 1);
            assert_eq!(doc.links[0].target, "other.md");
        }

        #[test]
        fn document_by_path_missing_returns_none() {
            let (_tmp, root) = synth_vault();
            let cache = populate_cache(&root);

            let result = cache
                .document_by_path(camino::Utf8Path::new("nope.md"))
                .unwrap();

            assert!(result.is_none());
        }

        #[test]
        fn has_diagnostic_errors_false_for_clean_vault() {
            let (_tmp, root) = synth_vault();
            let cache = populate_cache(&root);

            assert!(!cache.has_diagnostic_errors().unwrap());
        }

        #[test]
        fn has_diagnostic_errors_true_when_read_error_present() {
            let tmp = TempDir::new().unwrap();
            let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
                .unwrap()
                .join("vault");
            std::fs::create_dir(root.as_std_path()).unwrap();
            // Invalid UTF-8 bytes with a .md extension trip read_to_string,
            // which vault-frontmatter surfaces as a Severity::Error diagnostic
            // (code "read-failed").
            std::fs::write(
                root.join("bad-utf8.md").as_std_path(),
                b"\xff\xfe\xfd\xfc invalid utf-8 here",
            )
            .unwrap();
            let mut cache = Cache::open(&root).unwrap();
            cache.rebuild(&root).unwrap();

            assert!(cache.has_diagnostic_errors().unwrap());
        }

        #[test]
        fn frontmatter_in_set_any_of() {
            let (_tmp, root) = synth_vault();
            let cache = populate_cache(&root);

            let query = DocumentQuery {
                frontmatter_in: vec![(
                    "kind".to_string(),
                    vec![serde_json::json!("log"), serde_json::json!("meeting")],
                )],
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();

            // synth_vault has note-a.md (kind=log) and note-b.md (kind=meeting);
            // workspace.md has no kind; untyped.md has no frontmatter.
            assert_eq!(paths(&result), vec!["note-a.md", "note-b.md"]);
        }

        #[test]
        fn frontmatter_in_single_value_matches_eq() {
            let (_tmp, root) = synth_vault();
            let cache = populate_cache(&root);

            // `--in kind:log` with a single-element list should behave like `--eq kind:log`.
            let in_query = DocumentQuery {
                frontmatter_in: vec![("kind".to_string(), vec![serde_json::json!("log")])],
                ..Default::default()
            };
            let eq_query = DocumentQuery {
                frontmatter_eq: vec![("kind".to_string(), serde_json::json!("log"))],
                ..Default::default()
            };

            assert_eq!(
                paths(&cache.documents_matching(&in_query).unwrap()),
                paths(&cache.documents_matching(&eq_query).unwrap())
            );
        }

        #[test]
        fn frontmatter_not_in_set_excludes_listed_values() {
            let (_tmp, root) = synth_vault();
            let cache = populate_cache(&root);

            let query = DocumentQuery {
                frontmatter_not_in: vec![(
                    "type".to_string(),
                    vec![serde_json::json!("workspace")],
                )],
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();

            // type=workspace excluded; everything else (including docs without `type`) kept.
            // SQLite IN/NOT IN semantics with NULL: NULL is neither in nor not in any list.
            // Docs without `type` will have json_extract → NULL; NOT IN returns NULL (not TRUE).
            // So docs without `type` are excluded. Document this in the round-trip test.
            assert_eq!(paths(&result), vec!["note-a.md", "note-b.md"]);
        }

        #[test]
        fn frontmatter_in_combined_with_eq() {
            let (_tmp, root) = synth_vault();
            let cache = populate_cache(&root);

            let query = DocumentQuery {
                frontmatter_eq: vec![("type".to_string(), serde_json::json!("note"))],
                frontmatter_in: vec![(
                    "kind".to_string(),
                    vec![serde_json::json!("log"), serde_json::json!("meeting")],
                )],
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();

            assert_eq!(paths(&result), vec!["note-a.md", "note-b.md"]);
        }

        fn synth_dated_vault() -> (TempDir, Utf8PathBuf) {
            let tmp = TempDir::new().unwrap();
            let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
                .unwrap()
                .join("vault");
            std::fs::create_dir(root.as_std_path()).unwrap();
            std::fs::write(
                root.join("old.md").as_std_path(),
                "---\ncreated: 2025-01-15\n---\n",
            )
            .unwrap();
            std::fs::write(
                root.join("mid.md").as_std_path(),
                "---\ncreated: 2026-05-19\n---\n",
            )
            .unwrap();
            std::fs::write(
                root.join("new.md").as_std_path(),
                "---\ncreated: 2026-12-01\n---\n",
            )
            .unwrap();
            (tmp, root)
        }

        #[test]
        fn date_before_filters_chronologically() {
            let (_tmp, root) = synth_dated_vault();
            let mut cache = Cache::open(&root).unwrap();
            cache.rebuild(&root).unwrap();

            let query = DocumentQuery {
                date_before: vec![("created".to_string(), "2026-01-01".to_string())],
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();

            assert_eq!(paths(&result), vec!["old.md"]);
        }

        #[test]
        fn date_after_filters_chronologically() {
            let (_tmp, root) = synth_dated_vault();
            let mut cache = Cache::open(&root).unwrap();
            cache.rebuild(&root).unwrap();

            let query = DocumentQuery {
                date_after: vec![("created".to_string(), "2026-01-01".to_string())],
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();

            assert_eq!(paths(&result), vec!["mid.md", "new.md"]);
        }

        #[test]
        fn date_on_filters_exact_match() {
            let (_tmp, root) = synth_dated_vault();
            let mut cache = Cache::open(&root).unwrap();
            cache.rebuild(&root).unwrap();

            let query = DocumentQuery {
                date_on: vec![("created".to_string(), "2026-05-19".to_string())],
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();

            assert_eq!(paths(&result), vec!["mid.md"]);
        }

        #[test]
        fn date_predicates_compose_to_range() {
            let (_tmp, root) = synth_dated_vault();
            let mut cache = Cache::open(&root).unwrap();
            cache.rebuild(&root).unwrap();

            // 2026 only: after 2026-01-01 AND before 2026-12-31
            // mid.md=2026-05-19, new.md=2026-12-01 — both fall within the range.
            let query = DocumentQuery {
                date_after: vec![("created".to_string(), "2026-01-01".to_string())],
                date_before: vec![("created".to_string(), "2026-12-31".to_string())],
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();

            assert_eq!(paths(&result), vec!["mid.md", "new.md"]);
        }

        fn synth_text_vault() -> (TempDir, Utf8PathBuf) {
            let tmp = TempDir::new().unwrap();
            let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
                .unwrap()
                .join("vault");
            std::fs::create_dir(root.as_std_path()).unwrap();
            std::fs::write(
                root.join("sqlite.md").as_std_path(),
                "---\ntype: note\n---\nThis note discusses SQLite cache design.\n",
            )
            .unwrap();
            std::fs::write(
                root.join("rust.md").as_std_path(),
                "---\ntype: note\n---\nThis note is about Rust generics.\n",
            )
            .unwrap();
            std::fs::write(
                root.join("both.md").as_std_path(),
                "---\ntype: note\n---\nThis note covers both Rust AND sqlite topics.\n",
            )
            .unwrap();
            (tmp, root)
        }

        #[test]
        fn body_text_substring_matches() {
            let (_tmp, root) = synth_text_vault();
            let mut cache = Cache::open(&root).unwrap();
            cache.rebuild(&root).unwrap();

            let query = DocumentQuery {
                body_text_contains: Some("SQLite".to_string()),
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();

            // Case-insensitive — both "SQLite" (sqlite.md) and "sqlite" (both.md) match.
            assert_eq!(paths(&result), vec!["both.md", "sqlite.md"]);
        }

        #[test]
        fn body_text_case_insensitive_lowercase_needle() {
            let (_tmp, root) = synth_text_vault();
            let mut cache = Cache::open(&root).unwrap();
            cache.rebuild(&root).unwrap();

            let query = DocumentQuery {
                body_text_contains: Some("sqlite".to_string()),
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();

            // Lowercase needle matches both casings.
            assert_eq!(paths(&result), vec!["both.md", "sqlite.md"]);
        }

        #[test]
        fn body_text_combined_with_metadata() {
            let (_tmp, root) = synth_text_vault();
            let mut cache = Cache::open(&root).unwrap();
            cache.rebuild(&root).unwrap();

            let query = DocumentQuery {
                frontmatter_eq: vec![("type".to_string(), serde_json::json!("note"))],
                body_text_contains: Some("Rust".to_string()),
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();

            assert_eq!(paths(&result), vec!["both.md", "rust.md"]);
        }

        #[test]
        fn body_text_no_matches_returns_empty() {
            let (_tmp, root) = synth_text_vault();
            let mut cache = Cache::open(&root).unwrap();
            cache.rebuild(&root).unwrap();

            let query = DocumentQuery {
                body_text_contains: Some("nonexistent-keyword-xyzzy".to_string()),
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();

            assert_eq!(result.len(), 0);
        }

        // ── link-relationship predicates ──────────────────────────────────
        //
        // Vault shape:
        //   hub.md, side.md   — link targets (no outgoing links)
        //   both.md (task)    — links [[hub]] and [[side]]
        //   hubonly.md (note) — links [[hub]]
        //   broken.md (note)  — links [[ghost]] (unresolved)
        //   clean.md          — no links
        fn synth_linked_vault() -> (TempDir, Utf8PathBuf) {
            let tmp = TempDir::new().unwrap();
            let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
                .unwrap()
                .join("vault");
            std::fs::create_dir(root.as_std_path()).unwrap();
            std::fs::write(
                root.join("hub.md").as_std_path(),
                "---\ntype: note\n---\nhub body\n",
            )
            .unwrap();
            std::fs::write(
                root.join("side.md").as_std_path(),
                "---\ntype: note\n---\nside body\n",
            )
            .unwrap();
            std::fs::write(
                root.join("both.md").as_std_path(),
                "---\ntype: task\n---\nsee [[hub]] and [[side]]\n",
            )
            .unwrap();
            std::fs::write(
                root.join("hubonly.md").as_std_path(),
                "---\ntype: note\n---\nsee [[hub]]\n",
            )
            .unwrap();
            std::fs::write(
                root.join("broken.md").as_std_path(),
                "---\ntype: note\n---\nsee [[ghost]]\n",
            )
            .unwrap();
            std::fs::write(
                root.join("clean.md").as_std_path(),
                "---\ntype: note\n---\nno links\n",
            )
            .unwrap();
            (tmp, root)
        }

        #[test]
        fn links_to_returns_resolved_linkers() {
            let (_tmp, root) = synth_linked_vault();
            let cache = populate_cache(&root);

            // Derive the target path via the same resolver the CLI uses, to
            // confirm its output is byte-identical to the stored resolved_path.
            let resolved = crate::show::target::resolve_target(&cache, "hub").unwrap();
            assert_eq!(resolved.paths, vec![Utf8PathBuf::from("hub.md")]);

            let query = DocumentQuery {
                links_to: resolved.paths,
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();
            // both.md and hubonly.md link to hub; broken/clean/side/hub do not.
            assert_eq!(paths(&result), vec!["both.md", "hubonly.md"]);
        }

        #[test]
        fn links_to_multiple_targets_are_anded() {
            let (_tmp, root) = synth_linked_vault();
            let cache = populate_cache(&root);

            let query = DocumentQuery {
                links_to: vec![Utf8PathBuf::from("hub.md"), Utf8PathBuf::from("side.md")],
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();
            // Only both.md links to BOTH hub and side.
            assert_eq!(paths(&result), vec!["both.md"]);
        }

        #[test]
        fn links_to_excludes_dangling_only_linker() {
            let (_tmp, root) = synth_linked_vault();
            let cache = populate_cache(&root);

            // broken.md's only link is unresolved ([[ghost]]); it is never
            // returned by a resolved-only --links-to query for any target.
            let query = DocumentQuery {
                links_to: vec![Utf8PathBuf::from("hub.md")],
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();
            assert!(!paths(&result).contains(&"broken.md"));
        }

        #[test]
        fn unresolved_links_returns_docs_with_dangling_links() {
            let (_tmp, root) = synth_linked_vault();
            let cache = populate_cache(&root);

            let query = DocumentQuery {
                has_unresolved_links: true,
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();
            // Only broken.md has an unresolved link.
            assert_eq!(paths(&result), vec!["broken.md"]);
        }

        #[test]
        fn links_to_composes_with_frontmatter() {
            let (_tmp, root) = synth_linked_vault();
            let cache = populate_cache(&root);

            let query = DocumentQuery {
                links_to: vec![Utf8PathBuf::from("hub.md")],
                frontmatter_eq: vec![("type".to_string(), serde_json::json!("task"))],
                ..Default::default()
            };
            let result = cache.documents_matching(&query).unwrap();
            // both.md (task) links to hub; hubonly.md (note) is filtered out.
            assert_eq!(paths(&result), vec!["both.md"]);
        }

        #[test]
        fn links_to_query_plan_uses_resolved_index() {
            let (_tmp, root) = synth_linked_vault();
            let cache = populate_cache(&root);

            let conn = cache.conn();
            let mut stmt = conn
                .prepare(
                    "EXPLAIN QUERY PLAN \
                     SELECT path FROM documents \
                     WHERE path IN (SELECT source_path FROM links WHERE resolved_path = ?)",
                )
                .unwrap();
            let rows: Vec<String> = stmt
                .query_map(["hub.md"], |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();

            // The inner subquery must SEARCH links via idx_links_resolved, not
            // scan per-row. (A non-correlated LIST SUBQUERY is expected here.)
            assert!(
                rows.iter().any(|r| r.contains("idx_links_resolved")),
                "links-to plan does not use idx_links_resolved: {rows:?}"
            );
            assert!(
                !rows.iter().any(|r| r.contains("CORRELATED")),
                "links-to plan has a correlated subquery: {rows:?}"
            );
        }
    }
}
