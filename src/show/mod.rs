//! `norn get` — single-doc detail with multi-target support and
//! wikilink-aware input resolution.

pub mod render;
pub mod target;

use crate::cache::{Cache, IncomingLink};
use crate::core::{Heading, Link};
use anyhow::Result;
use serde::Serialize;

use crate::cli::GetArgs;

#[derive(Debug, Serialize)]
pub struct ShowRecord {
    pub path: camino::Utf8PathBuf,
    /// Filename stem. Carried for the opt-in `.stem` facet and `--sort stem`;
    /// skipped from the default serialize so `get --format json` (no `--col`)
    /// stays byte-identical and stem surfaces only when `--col .stem` asks.
    #[serde(skip)]
    pub stem: String,
    pub frontmatter: Option<serde_json::Value>,
    pub headings: Vec<Heading>,
    pub outgoing_links: Vec<Link>,
    pub unresolved_links: Vec<Link>,
    pub incoming_links: Vec<IncomingLink>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ShowReport {
    pub records: Vec<ShowRecord>,
    /// Non-fatal notes: ambiguous-stem warnings, missing-target errors.
    /// Routed to stderr by the caller. Skipped in JSON output.
    #[serde(skip)]
    pub notes: Vec<String>,
}

pub fn run(cache: &Cache, args: &GetArgs) -> Result<ShowReport> {
    let mut records: Vec<ShowRecord> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    // A requested heavy facet must load itself, independent of any flag.
    // `.body` (cache-served) loads when `--all-cols` (full dump) OR `.body`
    // is requested; `.raw` (disk read) loads when `.raw` is requested.
    // `--all-cols` is cache-only by design, so it never triggers a `.raw` read.
    let (facets, _fields) = crate::output::projection::split_cols(&args.col);
    let wants_body = args.all_cols || facets.iter().any(|f| f == "body");
    let wants_raw = facets.iter().any(|f| f == "raw");

    for raw in &args.targets {
        let resolved = target::resolve_target(cache, raw)?;
        if resolved.paths.is_empty() {
            notes.push(format!("error: '{}' did not resolve to any doc", raw));
            continue;
        }
        if resolved.paths.len() > 1 {
            notes.push(format!(
                "note: '{}' resolved to {} docs",
                raw,
                resolved.paths.len()
            ));
        }
        for path in &resolved.paths {
            let Some(deep) = cache.document_with_connections(path.as_path(), wants_body)? else {
                notes.push(format!(
                    "error: '{}' missing from cache after resolution",
                    path
                ));
                continue;
            };
            let raw = if wants_raw {
                crate::output::projection::read_raw(&cache.vault_root, &deep.path)
            } else {
                None
            };
            records.push(ShowRecord {
                path: deep.path,
                stem: deep.stem,
                frontmatter: deep.frontmatter,
                headings: deep.headings,
                outgoing_links: deep.outgoing_links,
                unresolved_links: deep.unresolved_links,
                incoming_links: deep.incoming_links,
                body: deep.body,
                raw,
            });
        }
    }

    // Sort / limit / paging are applied in-memory, post-resolution. get's sort
    // is a simple display-string field compare (not find's SQL collation) —
    // acceptable divergence for a targeted, already-resolved record set.
    // For `markdown` they're irrelevant (it returns a single byte-faithful
    // doc and still errors on >1 selected); skip so limit can't mask that.
    if !matches!(args.format, crate::cli::GetFormat::Markdown) {
        apply_sort(&mut records, args.paging.sort.as_deref(), args.paging.desc);
        apply_paging(&mut records, args.paging.starts_at, args.paging.limit);
    }

    Ok(ShowReport { records, notes })
}

/// Stably sort `records` by `field` (a frontmatter key or the identity `path`).
/// Records missing the field sort last. `desc` reverses the comparison.
fn apply_sort(records: &mut [ShowRecord], field: Option<&str>, desc: bool) {
    let Some(field) = field else { return };

    // Key for a record: Some(display string) when the field is present, None
    // when absent (absent sorts last regardless of direction).
    let key = |r: &ShowRecord| -> Option<String> {
        if field == "path" {
            return Some(r.path.as_str().to_string());
        }
        if field == "stem" {
            return Some(r.stem.clone());
        }
        r.frontmatter
            .as_ref()
            .and_then(|fm| fm.as_object())
            .and_then(|obj| obj.get(field))
            .map(crate::output::projection::json_value_inline)
    };

    records.sort_by(|a, b| {
        let (ka, kb) = (key(a), key(b));
        let ord = match (&ka, &kb) {
            (Some(x), Some(y)) => x.cmp(y),
            (Some(_), None) => std::cmp::Ordering::Less, // present before absent
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        };
        // Only invert the present/present comparison on --desc; absent always
        // sorts last in both directions.
        match (&ka, &kb) {
            (Some(_), Some(_)) if desc => ord.reverse(),
            _ => ord,
        }
    });
}

/// Apply the 1-indexed `starts_at` offset, then an optional `limit`, as an
/// in-memory slice of the (possibly sorted) records.
fn apply_paging(records: &mut Vec<ShowRecord>, starts_at: usize, limit: Option<usize>) {
    let offset = starts_at.saturating_sub(1);
    if offset > 0 {
        records.drain(..offset.min(records.len()));
    }
    if let Some(n) = limit {
        records.truncate(n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    fn synth_pair() -> (TempDir, Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-show-run-")
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

    fn open(root: &Utf8PathBuf) -> Cache {
        let mut cache = Cache::open(root).unwrap();
        cache.rebuild(root).unwrap();
        cache
    }

    fn args(targets: Vec<&str>, all_cols: bool) -> crate::cli::GetArgs {
        crate::cli::GetArgs {
            targets: targets.into_iter().map(String::from).collect(),
            all_cols,
            col: vec![],
            format: crate::cli::GetFormat::Records,
            paging: crate::cli::SortPaginateArgs {
                sort: None,
                desc: false,
                limit: None,
                no_limit: false,
                starts_at: 1,
            },
        }
    }

    #[test]
    fn single_target_returns_one_record() {
        let (_t, root) = synth_pair();
        let cache = open(&root);
        let r = run(&cache, &args(vec!["a.md"], false)).unwrap();
        assert_eq!(r.records.len(), 1);
        assert_eq!(r.records[0].path.as_str(), "a.md");
        assert!(r.records[0].body.is_none());
    }

    #[test]
    fn wikilink_target_resolves() {
        let (_t, root) = synth_pair();
        let cache = open(&root);
        let r = run(&cache, &args(vec!["[[a]]"], false)).unwrap();
        assert_eq!(r.records.len(), 1);
        assert_eq!(r.records[0].path.as_str(), "a.md");
    }

    #[test]
    fn multi_target_returns_n_records() {
        let (_t, root) = synth_pair();
        let cache = open(&root);
        let r = run(&cache, &args(vec!["a.md", "b.md"], false)).unwrap();
        assert_eq!(r.records.len(), 2);
    }

    #[test]
    fn all_cols_includes_body_content() {
        let (_t, root) = synth_pair();
        let cache = open(&root);
        let r = run(&cache, &args(vec!["a.md"], true)).unwrap();
        assert!(r.records[0].body.as_ref().unwrap().contains("A heading"));
    }

    #[test]
    fn missing_target_reports_in_notes_continues_others() {
        let (_t, root) = synth_pair();
        let cache = open(&root);
        let r = run(&cache, &args(vec!["a.md", "nonexistent", "b.md"], false)).unwrap();
        assert_eq!(r.records.len(), 2);
        assert!(r
            .notes
            .iter()
            .any(|n| n.contains("error:") && n.contains("nonexistent")));
    }

    #[test]
    fn col_narrows_to_named_field_only_in_json() {
        let (_t, root) = synth_pair();
        let cache = open(&root);
        let args = crate::cli::GetArgs {
            targets: vec!["a.md".to_string()],
            all_cols: false,
            paging: crate::cli::SortPaginateArgs {
                sort: None,
                desc: false,
                limit: None,
                no_limit: false,
                starts_at: 1,
            },
            col: vec![".incoming_links".to_string()],
            format: crate::cli::GetFormat::Json,
        };
        let r = run(&cache, &args).unwrap();
        let json = render::render_json_with_col(&r, &args.col);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        // Always-array shape; one record.
        let record = &v[0];
        assert!(record.get("incoming_links").is_some());
        assert!(record.get("frontmatter").is_none());
        assert!(record.get("outgoing_links").is_none());
        assert!(record.get("headings").is_none());
    }

    #[test]
    fn col_with_multiple_fields() {
        let (_t, root) = synth_pair();
        let cache = open(&root);
        let args = crate::cli::GetArgs {
            targets: vec!["a.md".to_string()],
            all_cols: false,
            paging: crate::cli::SortPaginateArgs {
                sort: None,
                desc: false,
                limit: None,
                no_limit: false,
                starts_at: 1,
            },
            col: vec![".headings".to_string(), ".outgoing_links".to_string()],
            format: crate::cli::GetFormat::Json,
        };
        let r = run(&cache, &args).unwrap();
        let json = render::render_json_with_col(&r, &args.col);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let record = &v[0];
        assert!(record.get("headings").is_some());
        assert!(record.get("outgoing_links").is_some());
        assert!(record.get("incoming_links").is_none());
    }

    #[test]
    fn json_default_includes_all_fields() {
        let (_t, root) = synth_pair();
        let cache = open(&root);
        let args = crate::cli::GetArgs {
            targets: vec!["a.md".to_string()],
            all_cols: false,
            paging: crate::cli::SortPaginateArgs {
                sort: None,
                desc: false,
                limit: None,
                no_limit: false,
                starts_at: 1,
            },
            col: vec![],
            format: crate::cli::GetFormat::Json,
        };
        let r = run(&cache, &args).unwrap();
        let json = render::render_json(&r);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let record = &v[0];
        assert!(record.get("path").is_some());
        assert!(record.get("frontmatter").is_some());
        assert!(record.get("headings").is_some());
        assert!(record.get("outgoing_links").is_some());
        assert!(record.get("unresolved_links").is_some());
        assert!(record.get("incoming_links").is_some());
        // body absent when not requested (skip_serializing_if = Option::is_none on the struct field)
        assert!(record.get("body").is_none());
    }

    #[test]
    fn text_records_block_emits_path_and_headings() {
        let (_t, root) = synth_pair();
        let cache = open(&root);
        let args = crate::cli::GetArgs {
            targets: vec!["a.md".to_string()],
            all_cols: false,
            paging: crate::cli::SortPaginateArgs {
                sort: None,
                desc: false,
                limit: None,
                no_limit: false,
                starts_at: 1,
            },
            col: vec![],
            format: crate::cli::GetFormat::Records,
        };
        let r = run(&cache, &args).unwrap();
        let text = render::render_records(&r);
        assert!(text.contains("a.md"), "expected path in output: {text:?}");
        assert!(
            text.contains("A heading"),
            "expected heading text in output: {text:?}"
        );
    }

    #[test]
    fn col_with_unknown_field_warns_but_does_not_error() {
        let (_t, root) = synth_pair();
        let cache = open(&root);
        let args = crate::cli::GetArgs {
            targets: vec!["a.md".to_string()],
            all_cols: false,
            paging: crate::cli::SortPaginateArgs {
                sort: None,
                desc: false,
                limit: None,
                no_limit: false,
                starts_at: 1,
            },
            col: vec!["nonexistent_field".to_string()],
            format: crate::cli::GetFormat::Json,
        };
        let r = run(&cache, &args).unwrap();
        // run() doesn't have stderr access; the warning fires at the render
        // layer. Just verify the run succeeded and emitted a record.
        assert_eq!(r.records.len(), 1);
        // The render layer's warning is tested separately or via the
        // integration test in tests/get_command.rs.
    }

    #[test]
    fn text_separator_between_multi_target_records() {
        let (_t, root) = synth_pair();
        let cache = open(&root);
        let args = crate::cli::GetArgs {
            targets: vec!["a.md".to_string(), "b.md".to_string()],
            all_cols: false,
            paging: crate::cli::SortPaginateArgs {
                sort: None,
                desc: false,
                limit: None,
                no_limit: false,
                starts_at: 1,
            },
            col: vec![],
            format: crate::cli::GetFormat::Records,
        };
        let r = run(&cache, &args).unwrap();
        let text = render::render_records(&r);
        // Both paths must appear.
        assert!(text.contains("a.md"), "expected a.md in output: {text:?}");
        assert!(text.contains("b.md"), "expected b.md in output: {text:?}");
        // primitives::separator() emits a line of '─' characters (U+2500).
        // Verify at least one such character is present to confirm the
        // separator was emitted between the two records.
        assert!(
            text.contains('─'),
            "expected separator (─) between records: {text:?}"
        );
    }

    // ---- sort / limit / paging (in-memory, post-resolution) ----

    fn rec(path: &str, fm: serde_json::Value) -> ShowRecord {
        let path = Utf8PathBuf::from(path);
        let stem = path.file_stem().unwrap_or_default().to_string();
        ShowRecord {
            stem,
            path,
            frontmatter: Some(fm),
            headings: vec![],
            outgoing_links: vec![],
            unresolved_links: vec![],
            incoming_links: vec![],
            body: None,
            raw: None,
        }
    }

    #[test]
    fn sort_orders_records_by_frontmatter_field() {
        let mut records = vec![
            rec("a.md", serde_json::json!({"order": "c"})),
            rec("b.md", serde_json::json!({"order": "a"})),
            rec("c.md", serde_json::json!({"order": "b"})),
        ];
        apply_sort(&mut records, Some("order"), false);
        let paths: Vec<&str> = records.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["b.md", "c.md", "a.md"]);
    }

    #[test]
    fn sort_desc_reverses() {
        let mut records = vec![
            rec("a.md", serde_json::json!({"order": "a"})),
            rec("b.md", serde_json::json!({"order": "b"})),
        ];
        apply_sort(&mut records, Some("order"), true);
        let paths: Vec<&str> = records.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["b.md", "a.md"]);
    }

    #[test]
    fn sort_by_path_identity() {
        let mut records = vec![
            rec("c.md", serde_json::json!({})),
            rec("a.md", serde_json::json!({})),
            rec("b.md", serde_json::json!({})),
        ];
        apply_sort(&mut records, Some("path"), false);
        let paths: Vec<&str> = records.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["a.md", "b.md", "c.md"]);
    }

    #[test]
    fn sort_by_stem_identity() {
        // Paths chosen so stem order (a, b, c) differs from path order — proves
        // the sort keys on the stem, not the full path.
        let mut records = vec![
            rec("z/a.md", serde_json::json!({})),
            rec("a/b.md", serde_json::json!({})),
            rec("m/c.md", serde_json::json!({})),
        ];
        apply_sort(&mut records, Some("stem"), false);
        let paths: Vec<&str> = records.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["z/a.md", "a/b.md", "m/c.md"]);
    }

    #[test]
    fn sort_missing_field_sorts_last() {
        let mut records = vec![
            rec("a.md", serde_json::json!({})),
            rec("b.md", serde_json::json!({"order": "z"})),
        ];
        apply_sort(&mut records, Some("order"), false);
        let paths: Vec<&str> = records.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["b.md", "a.md"], "absent field sorts last");
    }

    #[test]
    fn limit_truncates() {
        let mut records = vec![
            rec("a.md", serde_json::json!({})),
            rec("b.md", serde_json::json!({})),
            rec("c.md", serde_json::json!({})),
        ];
        apply_paging(&mut records, 1, Some(2));
        let paths: Vec<&str> = records.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["a.md", "b.md"]);
    }

    #[test]
    fn no_limit_returns_all() {
        let mut records = vec![
            rec("a.md", serde_json::json!({})),
            rec("b.md", serde_json::json!({})),
        ];
        apply_paging(&mut records, 1, None);
        assert_eq!(records.len(), 2);
    }

    #[test]
    fn starts_at_offsets() {
        let mut records = vec![
            rec("a.md", serde_json::json!({})),
            rec("b.md", serde_json::json!({})),
            rec("c.md", serde_json::json!({})),
        ];
        apply_paging(&mut records, 2, None);
        let paths: Vec<&str> = records.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["b.md", "c.md"]);
    }

    #[test]
    fn starts_at_then_limit() {
        let mut records = vec![
            rec("a.md", serde_json::json!({})),
            rec("b.md", serde_json::json!({})),
            rec("c.md", serde_json::json!({})),
            rec("d.md", serde_json::json!({})),
        ];
        apply_paging(&mut records, 2, Some(2));
        let paths: Vec<&str> = records.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["b.md", "c.md"]);
    }
}
