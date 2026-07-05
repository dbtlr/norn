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
    /// Full-content blake3 hash, for the opt-in `.document_hash` facet. Populated
    /// (and serialized) ONLY when `.document_hash` is requested — the same
    /// load-on-request + `skip_serializing_if` pattern as `.body`/`.raw`, so
    /// `get --format json` (no `--col`) stays byte-identical AND an MCP
    /// `vault.get` (which serializes the record directly) surfaces the hash when
    /// asked. `#[serde(skip)]` would hide it from the MCP envelope entirely.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_hash: Option<String>,
    pub frontmatter: Option<serde_json::Value>,
    pub headings: Vec<Heading>,
    pub outgoing_links: Vec<Link>,
    pub unresolved_links: Vec<Link>,
    pub incoming_links: Vec<IncomingLink>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// `--section` facet: one entry per requested heading that resolved
    /// uniquely, in request order, as `(heading, content)` where `content` is
    /// the exact byte span `edit::transform::resolve_section` covers (the
    /// heading line through end-of-section) — the SAME string in every output
    /// format, so a value read here round-trips to `edit --replace-section`.
    /// Headings that were missing or ambiguous in this document are omitted (a
    /// warning lands in `ShowReport::notes` instead). `None` when `--section`
    /// was not passed (omitted from JSON, like `.body`/`.raw`); `Some(vec![])`
    /// when it was passed but zero requested headings resolved for this
    /// document.
    ///
    /// Ordering: the `records`/TTY renderer iterates this `Vec` directly, so it
    /// honors REQUEST order. The JSON/JSONL/MCP paths funnel through
    /// `serde_json::Value` (a `BTreeMap`, no `preserve_order` feature), which
    /// keys the object alphabetically — so the JSON `sections` object is an
    /// unordered keyed lookup, NOT request-ordered. Both are built from
    /// [`sections_to_json_object`] via [`serialize_sections`].
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_sections"
    )]
    pub sections: Option<Vec<(String, String)>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
}

/// Build the `sections` JSON value: a plain object keyed by heading text
/// (`{heading: content, ...}`). Single-sourced so the `--col`-empty path (via
/// [`serialize_sections`] / `serde_json::to_value`) and the `--col`-narrowed
/// path (`render::narrow_to_json`) can't drift. Keys land in a
/// `serde_json::Map` (`BTreeMap` — no `preserve_order`), so the object is
/// alphabetically keyed regardless of insertion order.
pub(crate) fn sections_to_json_object(sections: &[(String, String)]) -> serde_json::Value {
    let mut obj = serde_json::Map::with_capacity(sections.len());
    for (heading, content) in sections {
        obj.insert(heading.clone(), serde_json::Value::String(content.clone()));
    }
    serde_json::Value::Object(obj)
}

/// `serialize_with` shim for the `sections` field: render it as the
/// [`sections_to_json_object`] object rather than serde's default
/// array-of-2-tuples for a `Vec<(String, String)>`. Only invoked when `Some`
/// (the field's `skip_serializing_if` filters out `None` first).
fn serialize_sections<S>(
    sections: &Option<Vec<(String, String)>>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::Serialize;
    sections_to_json_object(sections.as_deref().unwrap_or(&[])).serialize(serializer)
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
    // Whether the `body` FIELD should be displayed (the pre-existing
    // contract: opt-in via `--all-cols` or `--col .body`).
    let wants_body_display = args.all_cols || facets.iter().any(|f| f == "body");
    // `--section` is only CONSUMED by the formats that render it
    // (records/json/jsonl). `paths`/`markdown` document it as ignored, so we
    // must not resolve it for them — resolving would load the body and push
    // per-heading `warning:`/`error:` notes, and an `error:` note flips the
    // exit code, breaking a lookup a supposedly-ignored flag should leave
    // untouched. The `warn_section_ignored` stderr note (emitted by the
    // caller) is the honest "you passed --section but this format ignores it".
    let format_consumes_sections = matches!(
        args.format,
        crate::cli::GetFormat::Records | crate::cli::GetFormat::Json | crate::cli::GetFormat::Jsonl
    );
    // Requested headings, deduped preserving first-occurrence order — a user
    // can repeat `--section X`, and without dedup the JSON object (keyed)
    // collapses the duplicate while records/TTY would emit two blocks, a
    // cross-format cardinality mismatch. Empty when the format ignores
    // `--section`, which also keeps the body from loading for those formats.
    let requested_sections: Vec<String> = if format_consumes_sections {
        dedup_preserve_order(&args.section)
    } else {
        Vec::new()
    };
    // Whether the body needs to be LOADED from cache — additionally true when
    // `--section` will be resolved, which needs the body to resolve spans
    // against but does not (on its own) display the whole `body` field.
    // Loading is a superset of displaying; `wants_body_display` gates what
    // actually lands on `ShowRecord.body` below, so `--section` alone never
    // leaks the full body into output.
    let wants_body_load = wants_body_display || !requested_sections.is_empty();
    let wants_raw = facets.iter().any(|f| f == "raw");
    // `.document_hash` is identity/metadata-class: opt-in only (never in the
    // default or `--all-cols` dump), like `.raw`. Populated only when requested
    // so the default record stays byte-identical.
    let wants_document_hash = facets.iter().any(|f| f == "document_hash");

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
            let Some(deep) = cache.document_with_connections(path.as_path(), wants_body_load)?
            else {
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
            let sections = if requested_sections.is_empty() {
                None
            } else {
                Some(resolve_requested_sections(
                    deep.body.as_deref().unwrap_or(""),
                    &requested_sections,
                    &deep.path,
                    &mut notes,
                ))
            };
            // Only surface the whole-body field when it was independently
            // requested (`--all-cols`/`--col .body`) — a body loaded solely
            // to resolve `--section` spans must not leak into the default
            // dump.
            let body = if wants_body_display { deep.body } else { None };
            records.push(ShowRecord {
                path: deep.path,
                stem: deep.stem,
                // Omit when the hash is empty — an unreadable/unindexed file
                // carries `hash: ""` (graph::build), and handing a caller "" as
                // a CAS token would be misleading. Absent facet, like `.raw`
                // omits when the file is unreadable.
                document_hash: (wants_document_hash && !deep.hash.is_empty())
                    .then(|| deep.hash.clone()),
                frontmatter: deep.frontmatter,
                headings: deep.headings,
                outgoing_links: deep.outgoing_links,
                unresolved_links: deep.unresolved_links,
                incoming_links: deep.incoming_links,
                body,
                sections,
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

/// Resolve every `--section` heading against one document's `body`, reusing
/// `edit`'s `resolve_section` so a section READ mirrors a section WRITE —
/// identical byte spans, identical missing/ambiguous failure modes.
///
/// A heading that fails to resolve (missing or ambiguous in this document)
/// is warned to `notes` and omitted from the returned pairs — it does not
/// abort the document's other requested headings, nor sibling targets. If
/// NONE of `headings` resolve for this document, one additional `error:`
/// note is pushed so this target counts toward `get`'s existing nonzero-exit
/// contract (mirroring how a target that fails to resolve to any document at
/// all already behaves) — but the document's record is still returned, with
/// an empty `sections`, rather than dropped.
fn resolve_requested_sections(
    body: &str,
    headings: &[String],
    path: &camino::Utf8Path,
    notes: &mut Vec<String>,
) -> Vec<(String, String)> {
    // Parse the body's headings ONCE and resolve every requested span against
    // that single parse — `get`'s body is immutable across the whole call, so
    // (unlike `edit`, which re-parses per op because each op mutates the body)
    // there is no reason to re-parse per requested heading.
    let (parsed_headings, _links) = crate::links::parse_commonmark(path, body, body, 0);

    let mut resolved = Vec::with_capacity(headings.len());
    for heading in headings {
        // index/kind are edit's per-op bookkeeping (used only inside
        // EditError's Display); `get` has neither an op index nor an op
        // kind, so these are placeholders — we only branch on the error
        // variant, never surface these fields.
        match crate::edit::transform::resolve_section_in(
            &parsed_headings,
            body,
            heading,
            0,
            "get --section",
        ) {
            Ok(span) => {
                // The verbatim span — heading line through end-of-section,
                // trailing whitespace included. This exact string is what
                // every output format emits (json and records alike) and what
                // `edit --replace-section` round-trips against.
                resolved.push((
                    heading.clone(),
                    body[span.heading_start..span.end].to_string(),
                ));
            }
            Err(crate::edit::transform::EditError::HeadingNotFound { .. }) => {
                notes.push(format!(
                    "warning: --section heading '{heading}' not found in '{path}'"
                ));
            }
            Err(crate::edit::transform::EditError::HeadingAmbiguous { count, .. }) => {
                notes.push(format!(
                    "warning: --section heading '{heading}' is ambiguous ({count} matches) in '{path}'"
                ));
            }
            Err(other) => {
                // resolve_section only ever returns the two variants matched
                // above; any other variant would be a bug in that contract.
                notes.push(format!(
                    "warning: --section heading '{heading}' could not be resolved in '{path}': {other}"
                ));
            }
        }
    }
    if resolved.is_empty() {
        notes.push(format!(
            "error: none of the requested --section headings resolved for '{path}'"
        ));
    }
    resolved
}

/// Deduplicate `items` preserving first-occurrence order (a small
/// order-preserving `unique`). Used to collapse repeated `--section H`
/// occurrences so every output format agrees on cardinality.
fn dedup_preserve_order(items: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    items
        .iter()
        .filter(|item| seen.insert(item.as_str()))
        .cloned()
        .collect()
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
            section: vec![],
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
            section: vec![],
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
            section: vec![],
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
            section: vec![],
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
            section: vec![],
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
            section: vec![],
            format: crate::cli::GetFormat::Json,
        };
        let r = run(&cache, &args).unwrap();
        // run() doesn't have stderr access; the warning fires at the render
        // layer. Just verify the run succeeded and emitted a record.
        assert_eq!(r.records.len(), 1);
        // The render layer's warning is tested separately or via the
        // integration test in tests/get_command.rs.
    }

    // ---- `--section` (NRN-102) ----

    fn args_with_section(targets: Vec<&str>, section: Vec<&str>) -> crate::cli::GetArgs {
        args_with_section_fmt(targets, section, crate::cli::GetFormat::Json)
    }

    fn args_with_section_fmt(
        targets: Vec<&str>,
        section: Vec<&str>,
        format: crate::cli::GetFormat,
    ) -> crate::cli::GetArgs {
        crate::cli::GetArgs {
            targets: targets.into_iter().map(String::from).collect(),
            all_cols: false,
            paging: crate::cli::SortPaginateArgs {
                sort: None,
                desc: false,
                limit: None,
                no_limit: false,
                starts_at: 1,
            },
            col: vec![],
            section: section.into_iter().map(String::from).collect(),
            format,
        }
    }

    #[test]
    fn section_resolves_requested_heading_span_only() {
        let (_t, root) = synth_pair();
        std::fs::write(
            root.join("a.md").as_std_path(),
            "---\ntype: note\n---\n## One\nfirst\n## Two\nsecond\n",
        )
        .unwrap();
        let cache = open(&root);
        let r = run(&cache, &args_with_section(vec!["a.md"], vec!["One"])).unwrap();
        assert_eq!(r.records.len(), 1);
        let sections = r.records[0].sections.as_ref().unwrap();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].0, "One");
        assert_eq!(sections[0].1, "## One\nfirst\n");
        // The unrequested body facet does not leak in just because --section
        // forced a body load.
        assert!(r.records[0].body.is_none());
    }

    #[test]
    fn section_absent_when_not_requested() {
        let (_t, root) = synth_pair();
        let cache = open(&root);
        let r = run(&cache, &args_with_section(vec!["a.md"], vec![])).unwrap();
        assert!(r.records[0].sections.is_none());
    }

    #[test]
    fn section_missing_heading_warns_and_omits_but_siblings_resolve() {
        let (_t, root) = synth_pair();
        std::fs::write(
            root.join("a.md").as_std_path(),
            "---\ntype: note\n---\n## One\nfirst\n## Two\nsecond\n",
        )
        .unwrap();
        let cache = open(&root);
        let r = run(
            &cache,
            &args_with_section(vec!["a.md"], vec!["Missing", "One"]),
        )
        .unwrap();
        let sections = r.records[0].sections.as_ref().unwrap();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].0, "One");
        assert!(
            r.notes.iter().any(|n| n.starts_with("warning:")
                && n.contains("Missing")
                && n.contains("not found")),
            "expected a missing-heading warning, got: {:?}",
            r.notes
        );
        // Partial resolution is warn-not-fail: no `error:` note for this doc.
        assert!(!r.notes.iter().any(|n| n.starts_with("error:")));
    }

    #[test]
    fn section_ambiguous_heading_warns_and_omits() {
        let (_t, root) = synth_pair();
        std::fs::write(
            root.join("a.md").as_std_path(),
            "---\ntype: note\n---\n## Dup\nfirst\n## Dup\nsecond\n",
        )
        .unwrap();
        let cache = open(&root);
        let r = run(&cache, &args_with_section(vec!["a.md"], vec!["Dup"])).unwrap();
        assert!(r.records[0].sections.as_ref().unwrap().is_empty());
        assert!(r
            .notes
            .iter()
            .any(|n| n.starts_with("warning:") && n.contains("ambiguous") && n.contains("Dup")));
    }

    #[test]
    fn section_zero_resolved_for_target_is_hard_failure_note() {
        let (_t, root) = synth_pair();
        let cache = open(&root);
        // "a.md" has no such heading at all.
        let r = run(&cache, &args_with_section(vec!["a.md"], vec!["Nope"])).unwrap();
        // Record still returned — only the section facet is empty.
        assert_eq!(r.records.len(), 1);
        assert!(r.records[0].sections.as_ref().unwrap().is_empty());
        assert!(r
            .notes
            .iter()
            .any(|n| n.starts_with("error:") && n.contains("none of the requested")));
    }

    #[test]
    fn section_serializes_as_ordered_json_object_not_tuple_array() {
        let (_t, root) = synth_pair();
        std::fs::write(
            root.join("a.md").as_std_path(),
            "---\ntype: note\n---\n## One\nfirst\n## Two\nsecond\n",
        )
        .unwrap();
        let cache = open(&root);
        let r = run(&cache, &args_with_section(vec!["a.md"], vec!["One", "Two"])).unwrap();
        let json = render::render_json(&r);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let sections = v[0]["sections"]
            .as_object()
            .expect("sections is a JSON object, not an array of tuples");
        assert_eq!(
            sections.get("One").unwrap().as_str().unwrap(),
            "## One\nfirst\n"
        );
        assert_eq!(
            sections.get("Two").unwrap().as_str().unwrap(),
            "## Two\nsecond\n"
        );
    }

    #[test]
    fn section_not_resolved_for_paths_format() {
        // `paths` documents `--section` as ignored: run() must not resolve it
        // (no body load, no notes) so a missing heading can't push an `error:`
        // note that flips the exit code.
        let (_t, root) = synth_pair();
        let cache = open(&root);
        let r = run(
            &cache,
            &args_with_section_fmt(vec!["a.md"], vec!["Nope"], crate::cli::GetFormat::Paths),
        )
        .unwrap();
        assert_eq!(r.records.len(), 1);
        assert!(r.records[0].sections.is_none());
        assert!(
            r.notes.is_empty(),
            "no section notes for an ignored format, got: {:?}",
            r.notes
        );
    }

    #[test]
    fn section_not_resolved_for_markdown_format() {
        let (_t, root) = synth_pair();
        let cache = open(&root);
        let r = run(
            &cache,
            &args_with_section_fmt(vec!["a.md"], vec!["Nope"], crate::cli::GetFormat::Markdown),
        )
        .unwrap();
        assert!(r.records[0].sections.is_none());
        assert!(r.notes.is_empty(), "got: {:?}", r.notes);
    }

    #[test]
    fn section_duplicate_request_collapses_to_one() {
        // Repeated `--section One` must yield exactly one entry so json (keyed)
        // and records (per-block) agree on cardinality.
        let (_t, root) = synth_pair();
        std::fs::write(
            root.join("a.md").as_std_path(),
            "---\ntype: note\n---\n## One\nfirst\n## Two\nsecond\n",
        )
        .unwrap();
        let cache = open(&root);
        let r = run(&cache, &args_with_section(vec!["a.md"], vec!["One", "One"])).unwrap();
        let sections = r.records[0].sections.as_ref().unwrap();
        assert_eq!(sections.len(), 1, "duplicate heading must collapse to one");
        assert_eq!(sections[0].0, "One");
    }

    #[test]
    fn section_heading_with_comma_is_addressable() {
        // The read/write-parity headline: a heading text that itself contains
        // a comma is one whole `--section` value (no delimiter splitting), so
        // it resolves to exactly that section.
        let (_t, root) = synth_pair();
        std::fs::write(
            root.join("a.md").as_std_path(),
            "---\ntype: note\n---\n## Risks, Open Questions\nrisky\n## Next\nx\n",
        )
        .unwrap();
        let cache = open(&root);
        let r = run(
            &cache,
            &args_with_section(vec!["a.md"], vec!["Risks, Open Questions"]),
        )
        .unwrap();
        let sections = r.records[0].sections.as_ref().unwrap();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].0, "Risks, Open Questions");
        assert_eq!(sections[0].1, "## Risks, Open Questions\nrisky\n");
    }

    #[test]
    fn section_records_value_equals_json_value_verbatim() {
        // Fix 5: the records/TTY value must be byte-identical to the json span
        // (no trimming divergence) so either round-trips to edit.
        let (_t, root) = synth_pair();
        std::fs::write(
            root.join("a.md").as_std_path(),
            "---\ntype: note\n---\n## One\nfirst\n\n## Two\nsecond\n",
        )
        .unwrap();
        let cache = open(&root);
        let r = run(&cache, &args_with_section(vec!["a.md"], vec!["One"])).unwrap();
        // The stored value carries the section's trailing blank line verbatim.
        let stored = &r.records[0].sections.as_ref().unwrap()[0].1;
        assert_eq!(stored, "## One\nfirst\n\n");
        // json emits exactly that.
        let json = render::render_json(&r);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v[0]["sections"]["One"].as_str().unwrap(), stored);
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
            section: vec![],
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
            document_hash: None,
            frontmatter: Some(fm),
            headings: vec![],
            outgoing_links: vec![],
            unresolved_links: vec![],
            incoming_links: vec![],
            body: None,
            sections: None,
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
