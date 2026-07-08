//! CLI→service routing translation for `norn find` (NRN-222).
//!
//! `find` is routable byte-identically because the `vault.find` MCP tool's
//! [`FindOutput`](crate::mcp::tools::find::FindOutput) carries everything the CLI
//! renderers need: the `total`/`returned`/`truncated`/`starts_at` envelope
//! (NRN-214) and, per document, the SAME projected JSON `find --format json`
//! emits (`doc_to_json`). The client rebuilds a [`FindResult`] plus the parallel
//! deep-fetch / raw-read vectors from that payload and renders it through the
//! SAME `find::emit` seam the direct path uses, so routed and direct output are
//! byte-for-byte equal.
//!
//! **Why reconstruction is lossless for rendering.** `doc_to_json` (the JSON
//! projection) and `build_record_pairs` (the records projection) gate every
//! facet on the SAME `--col` / `--all-cols` split, so the wire carries a facet's
//! data exactly when the chosen renderer will read it. Fields the wire omits
//! (a document's `stem`, `hash`, `body_text` under a projection that doesn't ask
//! for them) are precisely the fields that renderer never reads, so defaulting
//! them is invisible in the output. `stem` — needed for `--col .stem` — is
//! `#[serde(skip)]` on the wire but is a pure function of the path
//! (`path.file_stem()`, exactly as `graph::build` computes it), so it is
//! re-derived rather than defaulted.
//!
//! Both functions here are pure so they unit-test without a live daemon; the
//! probe + wire round-trip live in the routing seam (`src/lib.rs`).

use anyhow::Result;
use camino::Utf8PathBuf;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::cache::{DocumentDeep, FindResult};
use crate::cli::FindArgs;
use crate::core::DocumentSummary;
use crate::output::projection::split_cols;

/// The reconstructed find result: the matched documents plus the parallel
/// deep-fetch / raw-read vectors, in the exact shape `find::emit` consumes,
/// plus the vault-level diagnostics bit the exit code derives from.
#[derive(Debug)]
pub struct RoutedFind {
    pub result: FindResult,
    pub deep: Vec<Option<DocumentDeep>>,
    pub raw: Vec<Option<String>>,
    /// Whether the vault carries any error-severity diagnostic — the daemon-side
    /// `cache.has_diagnostic_errors()`, crossing the wire so the routed path
    /// reproduces direct find's exit-2 contract (NRN-222).
    pub has_diagnostic_errors: bool,
}

/// Translate parsed `norn find` args into the `vault.find` tool's parameter
/// object (the `FindParams` shape in `src/mcp/tools/find.rs`).
///
/// `--format` / `--no-pager` / `--all` are deliberately absent: they are CLI-only
/// rendering / gating knobs (the client renders the returned structured data),
/// never query inputs.
pub fn to_mcp_arguments(args: &FindArgs) -> Value {
    let mut map = Map::new();

    let f = &args.filters;
    if let Some(text) = &f.text {
        map.insert("text".into(), Value::String(text.clone()));
    }
    insert_list(&mut map, "eq", &f.eq);
    insert_list(&mut map, "not_eq", &f.not_eq);
    insert_list(&mut map, "in", &f.r#in);
    insert_list(&mut map, "not_in", &f.not_in);
    insert_list(&mut map, "starts_with", &f.starts_with);
    insert_list(&mut map, "ends_with", &f.ends_with);
    insert_list(&mut map, "contains", &f.contains);
    insert_list(&mut map, "has", &f.has);
    insert_list(&mut map, "missing", &f.missing);
    insert_list(&mut map, "before", &f.before);
    insert_list(&mut map, "after", &f.after);
    insert_list(&mut map, "on", &f.on);
    insert_list(&mut map, "path", &f.path);
    insert_list(&mut map, "links_to", &f.links_to);
    if f.unresolved_links {
        map.insert("unresolved_links".into(), Value::Bool(true));
    }

    // Sort / limit / paging: name-for-name with the tool's params. An omitted
    // `--limit` is left absent so the tool applies find's default of 10 (matching
    // the direct path's `build_find_query`); an explicit `--limit`/`--no-limit`
    // travels through.
    let p = &args.paging;
    if let Some(sort) = &p.sort {
        map.insert("sort".into(), Value::String(sort.clone()));
    }
    if p.desc {
        map.insert("desc".into(), Value::Bool(true));
    }
    if let Some(limit) = p.limit {
        map.insert("limit".into(), Value::Number(limit.into()));
    }
    if p.no_limit {
        map.insert("no_limit".into(), Value::Bool(true));
    }
    // starts_at defaults to 1 on both surfaces; send it only when non-default to
    // keep the wire minimal (the tool floors at 1 either way).
    if p.starts_at != 1 {
        map.insert("starts_at".into(), Value::Number(p.starts_at.into()));
    }

    // Column projection: the tool applies the SAME `doc_to_json` projection the
    // CLI's `--format json` does, so the returned documents are already projected.
    if !args.col.is_empty() {
        map.insert(
            "col".into(),
            Value::Array(args.col.iter().cloned().map(Value::String).collect()),
        );
    }
    if args.all_cols {
        map.insert("all_cols".into(), Value::Bool(true));
    }

    Value::Object(map)
}

fn insert_list(map: &mut Map<String, Value>, key: &str, values: &[String]) {
    if !values.is_empty() {
        map.insert(
            key.into(),
            Value::Array(values.iter().cloned().map(Value::String).collect()),
        );
    }
}

/// Rebuild a [`RoutedFind`] from a `vault.find` `structuredContent` object.
///
/// The envelope (`total`/`returned`/`truncated`) rebuilds [`FindResult`]'s
/// counts; each `documents[i]` object is mapped back to a [`DocumentSummary`]
/// (plus a parallel [`DocumentDeep`] for the join-backed facets and a `.raw`
/// string), keyed off the same `--col`/`--all-cols` decision the direct
/// `find::query::select` makes — so `find::emit` renders the reconstruction
/// byte-identically to the direct path. Any shape mismatch is an `Err`, which the
/// caller maps to a verified direct open.
pub fn reconstruct(structured: &Value, args: &FindArgs) -> Result<RoutedFind> {
    let total = get_usize(structured, "total")?;
    let returned = get_usize(structured, "returned")?;
    let truncated = structured
        .get("truncated")
        .and_then(Value::as_bool)
        .ok_or_else(|| anyhow::anyhow!("find envelope missing bool `truncated`: {structured}"))?;
    // Required, not defaulted: the exit code derives from this bit, and guessing
    // it (e.g. an older daemon that predates the field) would silently break the
    // routed/direct exit-2 isomorphism — better to fall back to Direct.
    let has_diagnostic_errors = structured
        .get("has_diagnostic_errors")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            anyhow::anyhow!("find envelope missing bool `has_diagnostic_errors`: {structured}")
        })?;
    let documents = structured
        .get("documents")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("find envelope missing `documents` array: {structured}"))?;

    // Mirror `find::query::select`'s facet decisions so the reconstructed
    // deep/raw vectors are shaped exactly as the direct path's — empty when the
    // projection asks for no join-backed facet / no raw read.
    let (facets, _fields) = split_cols(&args.col);
    let needs_deep = args.all_cols
        || facets.iter().any(|f| {
            matches!(
                f.as_str(),
                "headings" | "outgoing_links" | "unresolved_links" | "incoming_links"
            )
        });
    let wants_raw = facets.iter().any(|f| f == "raw");

    let mut matches = Vec::with_capacity(documents.len());
    let mut deep: Vec<Option<DocumentDeep>> = Vec::new();
    let mut raw: Vec<Option<String>> = Vec::new();

    for doc in documents {
        let obj = doc
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("find document is not an object: {doc}"))?;
        let path = Utf8PathBuf::from(
            obj.get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("find document missing string `path`: {doc}"))?,
        );
        // `stem` is a pure function of the path (never on the wire); recompute it
        // exactly as `graph::build` does, so `--col .stem` renders identically.
        let stem = path.file_stem().unwrap_or_default().to_string();
        // `document_hash` is present only under `--col .document_hash` (and only
        // for a readable file); default empty — the renderer gates on non-empty.
        let hash = obj
            .get("document_hash")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let frontmatter = obj.get("frontmatter").cloned();
        let body_text = obj
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        matches.push(DocumentSummary {
            path: path.clone(),
            stem: stem.clone(),
            hash,
            frontmatter,
            body_text,
        });

        if needs_deep {
            // Only the four join-backed facet vectors are read off `deep` by the
            // renderers; the other `DocumentDeep` fields are inert here.
            deep.push(Some(DocumentDeep {
                path: path.clone(),
                stem,
                hash: String::new(),
                frontmatter: None,
                headings: take_vec(obj, "headings")?,
                outgoing_links: take_vec(obj, "outgoing_links")?,
                unresolved_links: take_vec(obj, "unresolved_links")?,
                incoming_links: take_vec(obj, "incoming_links")?,
                body: None,
            }));
        }
        if wants_raw {
            raw.push(obj.get("raw").and_then(Value::as_str).map(str::to_string));
        }
    }

    Ok(RoutedFind {
        result: FindResult {
            matches,
            total,
            returned,
            truncated,
        },
        deep,
        raw,
        has_diagnostic_errors,
    })
}

/// Deserialize `obj[key]` into `Vec<T>`, treating an absent key as an empty vec
/// (a facet the projection did not include).
fn take_vec<T: DeserializeOwned>(obj: &Map<String, Value>, key: &str) -> Result<Vec<T>> {
    match obj.get(key) {
        Some(value) => Ok(serde_json::from_value(value.clone())?),
        None => Ok(Vec::new()),
    }
}

fn get_usize(structured: &Value, key: &str) -> Result<usize> {
    Ok(structured
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("find envelope missing integer `{key}`: {structured}"))?
        as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::DocumentDeep;
    use crate::cli::{FindFormat, SortPaginateArgs};
    use crate::core::{DocumentSummary, Heading, Link, LinkKind, LinkStatus};
    use crate::filter_args::FilterArgs;
    use serde_json::json;

    fn base_args() -> FindArgs {
        FindArgs {
            filters: FilterArgs::default(),
            all: true,
            paging: SortPaginateArgs {
                sort: None,
                desc: false,
                limit: None,
                no_limit: false,
                starts_at: 1,
            },
            format: None,
            all_cols: false,
            col: vec![],
            no_pager: false,
        }
    }

    #[test]
    fn to_mcp_arguments_maps_filters_and_paging() {
        let mut args = base_args();
        args.filters.eq = vec!["type:note".into()];
        args.filters.text = Some("hello".into());
        args.paging.sort = Some("created".into());
        args.paging.desc = true;
        args.paging.limit = Some(5);
        args.col = vec!["title".into(), ".body".into()];

        let v = to_mcp_arguments(&args);
        assert_eq!(v["eq"], json!(["type:note"]));
        assert_eq!(v["text"], "hello");
        assert_eq!(v["sort"], "created");
        assert_eq!(v["desc"], true);
        assert_eq!(v["limit"], 5);
        assert_eq!(v["col"], json!(["title", ".body"]));
        // Defaults are omitted, not sent.
        assert!(v.get("no_limit").is_none());
        assert!(v.get("starts_at").is_none());
        assert!(v.get("all_cols").is_none());
    }

    // ── Round-trip isomorphism (NRN-222, per the count template) ──────────────
    //
    // The reconstruction is the exact inverse (for rendering purposes) of the
    // daemon's `vault.find` projection: build a `FindResult` + deep/raw, project
    // it to the wire `FindOutput` the tool serializes, reconstruct, and assert the
    // RENDERED bytes match the direct path — across formats and `--col`. (Struct
    // equality is deliberately NOT asserted: fields the projection omits, e.g. an
    // unread `hash`/`body_text`, are reconstructed as defaults and never read.)

    /// Project a `FindResult` + deep/raw to the `vault.find` wire envelope,
    /// exactly as `mcp::tools::find` does (`doc_to_json` per match + the count
    /// envelope), then serialize to the `structuredContent` JSON value.
    fn to_wire(
        result: &FindResult,
        deep: &[Option<DocumentDeep>],
        raw: &[Option<String>],
        args: &FindArgs,
    ) -> Value {
        let documents: Vec<Value> = result
            .matches
            .iter()
            .enumerate()
            .map(|(i, d)| {
                crate::find::render::doc_to_json(
                    d,
                    deep.get(i).and_then(|x| x.as_ref()),
                    raw.get(i).and_then(|x| x.as_deref()),
                    &args.col,
                    args.all_cols,
                )
            })
            .collect();
        json!({
            "total": result.total,
            "returned": result.returned,
            "truncated": result.truncated,
            "starts_at": args.paging.starts_at.max(1),
            "has_diagnostic_errors": false,
            "documents": documents,
        })
    }

    fn render_bytes(
        result: &FindResult,
        deep: &[Option<DocumentDeep>],
        raw: &[Option<String>],
        args: &FindArgs,
    ) -> (Vec<u8>, Vec<u8>) {
        let query = crate::find::query::build_find_query(args).unwrap();
        let format = args.format.unwrap_or(FindFormat::Records);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        crate::find::render::render(
            result,
            deep,
            raw,
            args,
            format,
            None,
            None,
            query.starts_at,
            &crate::output::palette::Palette::off(),
            &mut stdout,
            &mut stderr,
        )
        .unwrap();
        (stdout, stderr)
    }

    /// Assert routed (reconstructed) render bytes equal the direct render bytes
    /// for every output format, under the projection encoded in `args`.
    fn assert_round_trip(
        result: FindResult,
        deep: Vec<Option<DocumentDeep>>,
        raw: Vec<Option<String>>,
        mut args: FindArgs,
    ) {
        for format in [
            FindFormat::Paths,
            FindFormat::Records,
            FindFormat::Json,
            FindFormat::Jsonl,
        ] {
            args.format = Some(format);
            let wire = to_wire(&result, &deep, &raw, &args);
            let routed = reconstruct(&wire, &args).unwrap();

            let (direct_out, direct_err) = render_bytes(&result, &deep, &raw, &args);
            let (routed_out, routed_err) =
                render_bytes(&routed.result, &routed.deep, &routed.raw, &args);

            assert_eq!(
                direct_out, routed_out,
                "stdout must match for {format:?} (col={:?}, all_cols={})",
                args.col, args.all_cols
            );
            assert_eq!(
                direct_err, routed_err,
                "stderr must match for {format:?} (col={:?}, all_cols={})",
                args.col, args.all_cols
            );
        }
    }

    fn doc(path: &str, frontmatter: Value, body: &str) -> DocumentSummary {
        let path = Utf8PathBuf::from(path);
        DocumentSummary {
            stem: path.file_stem().unwrap_or_default().to_string(),
            hash: format!("hash-of-{path}"),
            frontmatter: Some(frontmatter),
            body_text: body.to_string(),
            path,
        }
    }

    fn sample_result() -> FindResult {
        FindResult {
            matches: vec![
                doc(
                    "note1.md",
                    json!({"type": "note", "title": "One"}),
                    "body one",
                ),
                doc(
                    "note2.md",
                    json!({"type": "note", "title": "Two"}),
                    "body two",
                ),
            ],
            total: 5,
            returned: 2,
            truncated: true,
        }
    }

    #[test]
    fn round_trip_default_projection() {
        let result = sample_result();
        assert_round_trip(result, vec![], vec![], base_args());
    }

    #[test]
    fn round_trip_bare_field_col() {
        let result = sample_result();
        let mut args = base_args();
        args.col = vec!["title".into()];
        assert_round_trip(result, vec![], vec![], args);
    }

    #[test]
    fn round_trip_missing_bare_field() {
        // A doc lacking the requested field must round-trip to the same
        // "(no matching fields)" placeholder / empty frontmatter object.
        let result = FindResult {
            matches: vec![doc("a.md", json!({"type": "note"}), "")],
            total: 1,
            returned: 1,
            truncated: false,
        };
        let mut args = base_args();
        args.col = vec!["nonexistent".into()];
        assert_round_trip(result, vec![], vec![], args);
    }

    #[test]
    fn round_trip_body_and_stem_and_hash_facets() {
        let result = sample_result();
        let mut args = base_args();
        args.col = vec![".body".into(), ".stem".into(), ".document_hash".into()];
        assert_round_trip(result, vec![], vec![], args);
    }

    fn deep_for(path: &str) -> DocumentDeep {
        let heading = Heading {
            level: 1,
            text: "Heading".into(),
            slug: "heading".into(),
            source_span: None,
        };
        let link = Link {
            source_path: Utf8PathBuf::from(path),
            raw: "[[target]]".into(),
            kind: LinkKind::Wikilink,
            target: "target".into(),
            label: None,
            anchor: None,
            block_ref: None,
            source_span: None,
            source_context: None,
            resolved_path: Some(Utf8PathBuf::from("target.md")),
            unresolved_reason: None,
            candidates: vec![],
            status: LinkStatus::Resolved,
        };
        DocumentDeep {
            path: Utf8PathBuf::from(path),
            stem: "x".into(),
            hash: String::new(),
            frontmatter: None,
            headings: vec![heading],
            outgoing_links: vec![link],
            unresolved_links: vec![],
            incoming_links: vec![],
            body: None,
        }
    }

    #[test]
    fn round_trip_deep_facets() {
        let result = sample_result();
        let deep = vec![Some(deep_for("note1.md")), Some(deep_for("note2.md"))];
        let mut args = base_args();
        args.col = vec![".headings".into(), ".outgoing_links".into()];
        assert_round_trip(result, deep, vec![], args);
    }

    #[test]
    fn round_trip_all_cols() {
        let result = sample_result();
        let deep = vec![Some(deep_for("note1.md")), Some(deep_for("note2.md"))];
        let mut args = base_args();
        args.all_cols = true;
        assert_round_trip(result, deep, vec![], args);
    }

    #[test]
    fn round_trip_raw_facet() {
        let result = sample_result();
        let raw = vec![
            Some("---\ntype: note\n---\nraw one\n".to_string()),
            Some("---\ntype: note\n---\nraw two\n".to_string()),
        ];
        let mut args = base_args();
        args.col = vec![".raw".into()];
        assert_round_trip(result, vec![], raw, args);
    }

    #[test]
    fn round_trip_empty_result() {
        let result = FindResult {
            matches: vec![],
            total: 0,
            returned: 0,
            truncated: false,
        };
        assert_round_trip(result, vec![], vec![], base_args());
    }

    // ── Exit-code isomorphism: the vault-diagnostics bit (NRN-222) ────────────

    /// The `has_diagnostic_errors` bit crosses the wire faithfully in both
    /// states — it is what `route_find` derives exit 2 vs 0 from.
    #[test]
    fn diagnostics_bit_round_trips() {
        let args = base_args();
        for bit in [false, true] {
            let mut wire = to_wire(&sample_result(), &[], &[], &args);
            wire["has_diagnostic_errors"] = json!(bit);
            let routed = reconstruct(&wire, &args).unwrap();
            assert_eq!(routed.has_diagnostic_errors, bit);
        }
    }

    /// A wire missing the diagnostics bit (e.g. an older daemon) must fail
    /// reconstruction, so the caller falls back to Direct instead of guessing
    /// the exit code.
    #[test]
    fn missing_diagnostics_bit_is_an_error() {
        let args = base_args();
        let mut wire = to_wire(&sample_result(), &[], &[], &args);
        wire.as_object_mut()
            .unwrap()
            .remove("has_diagnostic_errors");
        let err = reconstruct(&wire, &args).unwrap_err();
        assert!(
            err.to_string().contains("has_diagnostic_errors"),
            "got: {err}"
        );
    }

    /// End-to-end exit-2 isomorphism against a REAL vault with an error-severity
    /// diagnostic: the actual `vault.find` tool projection (not the test-local
    /// `to_wire`) carries the bit, reconstruction surfaces it, and the exit code
    /// each path derives — direct from `cache.has_diagnostic_errors()`, routed
    /// from the wire bit — is 2 on both.
    #[test]
    fn exit_code_matches_direct_on_diagnostic_error_vault() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-find-route-diag-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(
            root.join("good.md"),
            "---\ntype: note\ntitle: Good\n---\nbody\n",
        )
        .unwrap();
        // Invalid UTF-8 with a .md extension trips read_to_string, surfaced as a
        // Severity::Error diagnostic (code "read-failed").
        std::fs::write(
            root.join("bad-utf8.md").as_std_path(),
            b"\xff\xfe\xfd\xfc invalid utf-8 here",
        )
        .unwrap();

        // Daemon side: the REAL tool projection.
        let ctx = crate::mcp::context::VaultContext::open(&root, None).unwrap();
        let out = crate::mcp::tools::find::handle(
            &ctx,
            serde_json::from_value(json!({ "eq": ["type:note"] })).unwrap(),
        )
        .unwrap();
        assert!(
            out.has_diagnostic_errors,
            "tool envelope must carry the vault's error-diagnostic bit"
        );
        let wire = serde_json::to_value(&out).unwrap();

        // Client side: reconstruct and derive the exit code like `route_find`.
        let mut args = base_args();
        args.filters.eq = vec!["type:note".into()];
        let routed = reconstruct(&wire, &args).unwrap();
        let routed_exit = if routed.has_diagnostic_errors { 2 } else { 0 };

        // Direct side: the same signal `find::run` exits on.
        let cache =
            crate::cache_cmd::open_for_query(&root, &crate::graph::IndexOptions::default(), false)
                .unwrap();
        let direct_exit = if cache.has_diagnostic_errors().unwrap() {
            2
        } else {
            0
        };

        assert_eq!(direct_exit, 2, "fixture must carry an error diagnostic");
        assert_eq!(routed_exit, direct_exit, "exit codes must match");
    }
}
