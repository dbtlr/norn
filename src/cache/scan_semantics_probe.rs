//! Phase-1 semantics probe for the Wave-2 EAV query router (NRN-79).
//!
//! `build_documents_matching_sql_parts` in `query_documents.rs` is the
//! long-standing "scan path" — every predicate compiles to a `json_extract`
//! (or `json_each`) expression over `documents.frontmatter_json`. Its exact
//! behavior for non-string value shapes was never written down anywhere
//! except the SQL itself. This module builds tiny vaults, runs every
//! predicate class against them through `Cache::open` (non-authoritative —
//! the router never engages here, so these tests always exercise the scan
//! path regardless of what the router later does), and pins down what the
//! scan path ACTUALLY does — not what it "should" do.
//!
//! These pinned truths are the router's contract: a predicate class routes
//! through `document_fields` only when the router's compiled SQL is proven
//! (by these same truths) to reproduce the scan path byte-for-byte. Where a
//! truth here shows a divergence the EAV form can't reproduce, the router
//! falls back to the scan path for that predicate class — see the
//! `not_eav_provable_*` doc comments in `query_documents.rs` and this
//! task's report for the enumerated list.
//!
//! Do NOT "fix" a surprising assertion in this file — if the scan path's
//! behavior looks like a bug, pin it as-is and file it separately. Changing
//! an assertion here to what looks "more correct" silently changes what the
//! router is allowed to promise.

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    use crate::cache::{Cache, DocumentQuery};

    fn vault_with(docs: &[(&str, &str)]) -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        for (name, body) in docs {
            std::fs::write(root.join(name).as_std_path(), body).unwrap();
        }
        (tmp, root)
    }

    fn open(root: &Utf8PathBuf) -> Cache {
        let mut cache = Cache::open(root).unwrap();
        cache.rebuild(root).unwrap();
        cache
    }

    fn matched(cache: &Cache, query: DocumentQuery) -> Vec<String> {
        let mut paths: Vec<String> = cache
            .documents_matching(&query)
            .unwrap()
            .into_iter()
            .map(|d| d.path.to_string())
            .collect();
        paths.sort();
        paths
    }

    fn eq(field: &str, value: serde_json::Value) -> DocumentQuery {
        DocumentQuery {
            frontmatter_eq: vec![(field.to_string(), value)],
            ..Default::default()
        }
    }

    fn in_(field: &str, values: Vec<serde_json::Value>) -> DocumentQuery {
        DocumentQuery {
            frontmatter_in: vec![(field.to_string(), values)],
            ..Default::default()
        }
    }

    // ── wikilink string shapes ──────────────────────────────────────────
    //
    // `strip_wikilink_brackets` is a literal remove-all of "[[" / "]]"
    // substrings, not a balanced-pair parse. Pin exactly what that means
    // for the alias and embedded-brackets shapes, which are easy to
    // mis-assume.

    #[test]
    fn eq_string_wikilink_shapes() {
        let (_tmp, root) = vault_with(&[
            ("bare.md", "---\nf: \"[[X]]\"\n---\n"),
            ("alias.md", "---\nf: \"[[X|alias]]\"\n---\n"),
            ("embedded.md", "---\nf: \"a[[X]]b\"\n---\n"),
            ("plain.md", "---\nf: \"X\"\n---\n"),
        ]);
        let cache = open(&root);

        // Querying the bare target "X" matches the bracket-only and plain
        // forms — bracket-stripping is symmetric on both sides.
        assert_eq!(
            matched(&cache, eq("f", serde_json::json!("X"))),
            vec!["bare.md", "plain.md"],
            "querying X should match [[X]] and X, not the alias or embedded shapes"
        );

        // PINNED SURPRISE: "[[X|alias]]" strips to "X|alias" (the pipe is
        // NOT alias-parsed away) — querying "X" does NOT match it, and only
        // querying the literal "X|alias" text does.
        assert!(matched(&cache, eq("f", serde_json::json!("X|alias"))) == vec!["alias.md"]);

        // PINNED SURPRISE: "a[[X]]b" strips to "aXb" (remove-all, not a
        // balanced-pair strip) — querying "X" does not match; only "aXb" does.
        assert_eq!(
            matched(&cache, eq("f", serde_json::json!("aXb"))),
            vec!["embedded.md"]
        );
    }

    // ── non-string --eq is NOT array-aware ──────────────────────────────
    //
    // `push_equality`'s non-string branch is a bare
    // `json_extract(frontmatter_json, ?) = ?` with no array_each treatment
    // at all (unlike the string branch). This means a typed --eq can never
    // match inside an array, even a single-element array — this is the
    // reason non-string --eq/--in/--not-in fall back to the scan path
    // instead of routing through document_fields (which stores one row per
    // array element regardless of type, and so WOULD match).

    #[test]
    fn eq_integer_is_not_array_aware() {
        let (_tmp, root) = vault_with(&[
            ("scalar.md", "---\nn: 5\n---\n"),
            ("array.md", "---\nn: [5, 6]\n---\n"),
            ("singleton.md", "---\nn: [5]\n---\n"),
        ]);
        let cache = open(&root);
        assert_eq!(
            matched(&cache, eq("n", serde_json::json!(5))),
            vec!["scalar.md"],
            "integer --eq must not match array or singleton-array elements"
        );
    }

    #[test]
    fn eq_boolean_is_not_array_aware() {
        let (_tmp, root) = vault_with(&[
            ("scalar.md", "---\nb: true\n---\n"),
            ("array.md", "---\nb: [true, false]\n---\n"),
        ]);
        let cache = open(&root);
        assert_eq!(
            matched(&cache, eq("b", serde_json::json!(true))),
            vec!["scalar.md"]
        );
    }

    #[test]
    fn in_non_string_values_is_not_array_aware() {
        let (_tmp, root) = vault_with(&[
            ("scalar.md", "---\nn: 5\n---\n"),
            ("array.md", "---\nn: [5, 6]\n---\n"),
        ]);
        let cache = open(&root);
        // Mixed/non-string --in falls through to the same non-array-aware
        // `json_extract(...) IN (...)` form as scalar --eq.
        assert_eq!(
            matched(&cache, in_("n", vec![serde_json::json!(5)])),
            vec!["scalar.md"]
        );
    }

    #[test]
    fn in_string_values_is_array_aware() {
        // Contrast case: an all-string --in list DOES use the array-aware
        // form (existing behavior, already covered in query.rs property
        // tests) — restated here for side-by-side contrast with the
        // non-string case above.
        let (_tmp, root) = vault_with(&[
            ("scalar.md", "---\ns: five\n---\n"),
            ("array.md", "---\ns: [five, six]\n---\n"),
        ]);
        let cache = open(&root);
        assert_eq!(
            matched(&cache, in_("s", vec![serde_json::json!("five")])),
            vec!["array.md", "scalar.md"]
        );
    }

    // ── integer/float equality ───────────────────────────────────────────

    #[test]
    fn eq_integer_matches_stored_float_numerically() {
        // SQLite compares INTEGER and REAL numerically regardless of
        // storage class: 5 = 5.0 is true. Querying `--eq n:5` (parsed as
        // JSON integer 5) against a stored `5.0` (JSON float) matches.
        let (_tmp, root) = vault_with(&[("a.md", "---\nn: 5.0\n---\n")]);
        let cache = open(&root);
        assert_eq!(matched(&cache, eq("n", serde_json::json!(5))), vec!["a.md"]);
        assert_eq!(
            matched(&cache, eq("n", serde_json::json!(5.0))),
            vec!["a.md"]
        );
    }

    // ── boolean rendering diverges between --eq and string-ops ──────────
    //
    // json_value_to_sql (used by both the scan path's non-string --eq and
    // the document_fields writer's canonicalize_scalar) renders bool as
    // SQLite INTEGER 0/1. But `push_string_operator`'s CASE WHEN
    // deliberately overrides that for --starts-with/--ends-with/--contains,
    // rendering bool as the literal text "true"/"false" instead. The same
    // stored boolean therefore needs TWO different SQL representations
    // depending on which predicate touches it — document_fields can only
    // hold one canonical representation per row, so string-ops can never be
    // reproduced via the EAV table for a field that might ever hold a
    // boolean. This is why --starts-with/--ends-with/--contains always fall
    // back to the scan path in the router, regardless of index membership.

    #[test]
    fn string_op_renders_bool_as_source_text_not_integer() {
        let (_tmp, root) = vault_with(&[("a.md", "---\narchived: true\nflags:\n  - false\n---\n")]);
        let cache = open(&root);

        let contains_true = DocumentQuery {
            frontmatter_contains: vec![("archived".to_string(), "true".to_string())],
            ..Default::default()
        };
        assert_eq!(matched(&cache, contains_true), vec!["a.md"]);

        // The SQLite INTEGER rendering (1) must NOT leak into string-op
        // matching — this is the exact divergence from canonicalize_scalar,
        // which WOULD store this bool as SqlValue::Integer(1).
        let contains_one = DocumentQuery {
            frontmatter_contains: vec![("archived".to_string(), "1".to_string())],
            ..Default::default()
        };
        assert!(matched(&cache, contains_one).is_empty());

        let starts_with_false = DocumentQuery {
            frontmatter_starts_with: vec![("flags".to_string(), "fal".to_string())],
            ..Default::default()
        };
        assert_eq!(matched(&cache, starts_with_false), vec!["a.md"]);
    }

    // ── date ops are scalar-only: arrays compare as JSON array text ──────
    //
    // `date_before`/`date_after`/`date_on` push a bare
    // `json_extract(frontmatter_json, ?) OP ?` with NO array-awareness at
    // all (unlike --eq's string branch). For an array-valued field,
    // json_extract returns the whole array's JSON-encoded text (e.g.
    // `["2025-01-01"]`), which is compared as TEXT against the date-string
    // bind. Since document_fields stores one row per array ELEMENT (typed,
    // canonicalized), a naive EAV compilation of date ops would test each
    // element's date value individually — a completely different result
    // from comparing the whole array's JSON text. This divergence is why
    // date ops always fall back to the scan path in the router.

    #[test]
    fn date_before_on_array_field_compares_json_array_text_not_elements() {
        let (_tmp, root) = vault_with(&[
            // Every element is chronologically before the query date, but
            // the array's JSON text ("[...]") sorts AFTER any plain
            // date-string lexically ('[' > any ASCII digit) — the reverse
            // of what a per-element scan would produce.
            (
                "array.md",
                "---\ncreated:\n  - 2020-01-01\n  - 2021-01-01\n---\n",
            ),
            ("scalar.md", "---\ncreated: 2020-01-01\n---\n"),
        ]);
        let cache = open(&root);

        let before = DocumentQuery {
            date_before: vec![("created".to_string(), "2026-01-01".to_string())],
            ..Default::default()
        };
        assert_eq!(
            matched(&cache, before),
            vec!["scalar.md"],
            "array-valued date field must NOT match --before via per-element \
             comparison — the scan path compares the whole array's JSON text"
        );

        let after = DocumentQuery {
            date_after: vec![("created".to_string(), "2019-01-01".to_string())],
            ..Default::default()
        };
        // The array's JSON text sorts after any plain date string, so
        // --after matches it too (for the wrong reason — text ordering, not
        // date semantics). Pinning this exact quirk.
        assert_eq!(matched(&cache, after), vec!["array.md", "scalar.md"]);
    }

    #[test]
    fn date_ops_on_non_string_scalar_compare_by_sqlite_storage_class() {
        // json_extract returns INTEGER/REAL for numeric scalars; comparing
        // INTEGER/REAL to a TEXT bind follows SQLite's storage-class
        // ordering (NULL < INTEGER/REAL < TEXT < BLOB) — a numeric value is
        // always "less than" any text bind, never equal or greater.
        let (_tmp, root) = vault_with(&[("a.md", "---\ncreated: 20260101\n---\n")]);
        let cache = open(&root);

        let before = DocumentQuery {
            date_before: vec![("created".to_string(), "2020-01-01".to_string())],
            ..Default::default()
        };
        assert_eq!(
            matched(&cache, before),
            vec!["a.md"],
            "an INTEGER stored value is always < any TEXT date bind"
        );

        let after = DocumentQuery {
            date_after: vec![("created".to_string(), "2020-01-01".to_string())],
            ..Default::default()
        };
        assert!(matched(&cache, after).is_empty());

        let on = DocumentQuery {
            date_on: vec![("created".to_string(), "20260101".to_string())],
            ..Default::default()
        };
        assert!(
            matched(&cache, on).is_empty(),
            "INTEGER never equals a TEXT bind even with matching digits"
        );
    }

    // ── --has / --missing over the "no scalar" value shapes ─────────────

    #[test]
    fn has_and_missing_pinned_truths() {
        let (_tmp, root) = vault_with(&[
            ("empty_array.md", "---\ntags: []\n---\n"),
            ("all_null_array.md", "---\ntags: [null, null]\n---\n"),
            ("null_field.md", "---\ntags: null\n---\n"),
            ("missing_field.md", "---\nother: x\n---\n"),
            ("object_field.md", "---\ntags: {a: 1}\n---\n"),
            ("present.md", "---\ntags: [a]\n---\n"),
        ]);
        let cache = open(&root);

        let has = DocumentQuery {
            frontmatter_has: vec!["tags".to_string()],
            ..Default::default()
        };
        assert_eq!(
            matched(&cache, has),
            vec![
                "all_null_array.md",
                "empty_array.md",
                "object_field.md",
                "present.md",
            ],
            "empty array, all-null array, and object all count as present; \
             null-valued and missing do not"
        );

        let missing = DocumentQuery {
            frontmatter_missing: vec!["tags".to_string()],
            ..Default::default()
        };
        assert_eq!(
            matched(&cache, missing),
            vec!["missing_field.md", "null_field.md"]
        );
    }

    // ── empty string never collides with the absent-sentinel behavior ───

    #[test]
    fn eq_empty_string_matches_only_genuinely_empty_string_values() {
        let (_tmp, root) = vault_with(&[
            ("empty.md", "---\nf: \"\"\n---\n"),
            ("missing.md", "---\nother: x\n---\n"),
            ("null_field.md", "---\nf: null\n---\n"),
            ("present.md", "---\nf: x\n---\n"),
        ]);
        let cache = open(&root);
        assert_eq!(
            matched(&cache, eq("f", serde_json::json!(""))),
            vec!["empty.md"]
        );
    }

    // ── missing field never matches any string operator ──────────────────

    #[test]
    fn string_operator_on_missing_field_never_matches() {
        let (_tmp, root) = vault_with(&[("a.md", "---\nother: x\n---\n")]);
        let cache = open(&root);
        let contains = DocumentQuery {
            frontmatter_contains: vec![("nope".to_string(), "x".to_string())],
            ..Default::default()
        };
        assert!(matched(&cache, contains).is_empty());
    }

    // ── --not-eq excludes missing fields (needs --has to include them) ──

    #[test]
    fn not_eq_excludes_missing_field_by_default() {
        let (_tmp, root) = vault_with(&[
            ("present_other.md", "---\nf: y\n---\n"),
            ("missing.md", "---\nother: x\n---\n"),
        ]);
        let cache = open(&root);
        let not_eq = DocumentQuery {
            frontmatter_not_eq: vec![("f".to_string(), serde_json::json!("x"))],
            ..Default::default()
        };
        assert_eq!(
            matched(&cache, not_eq),
            vec!["present_other.md"],
            "missing.md lacks `f` entirely and must NOT be included by --not-eq alone"
        );
    }
}
