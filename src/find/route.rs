//! CLI→service routing translation for `norn find` (NRN-222).
//!
//! `find` is routable byte-identically because the `vault.find` MCP tool's
//! [`FindOutput`](crate::mcp::tools::find::FindOutput) carries everything the CLI
//! renderers need: the `total`/`returned`/`truncated`/`starts_at` envelope
//! (NRN-214) and, per document, the projected JSON `find --format json` emits
//! (`doc_to_wire_json` — `doc_to_json` plus the absent-vs-null frontmatter
//! distinction). The client rebuilds a [`FindResult`] plus the parallel
//! deep-fetch vector from that payload and renders it through the
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
use serde_json::{Map, Value};

use crate::cache::{DocumentDeep, FindResult};
use crate::cli::FindArgs;
use crate::core::DocumentSummary;
use crate::output::projection::split_cols;
use crate::route_wire::{
    get_bool, get_usize, insert_filter_args, insert_list, insert_paging, json_type, take_vec,
};

/// The reconstructed find result: the matched documents plus the parallel
/// deep-fetch vector, in the exact shape `find::emit` consumes,
/// plus the vault-level diagnostics bit the exit code derives from.
#[derive(Debug)]
pub struct RoutedFind {
    pub result: FindResult,
    pub deep: Vec<Option<DocumentDeep>>,
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
///
/// `dynamic_keys` are the field names the ADR 0010 forgiving-input pass desugared
/// from `--<field> value` spellings (NRN-218); they ride the private wire channel
/// so the daemon can run the field-universe gate against its warm cache. Empty
/// (the canonical spelling) omits the key, keeping the wire byte-identical.
pub fn to_mcp_arguments(args: &FindArgs, dynamic_keys: &[String]) -> Value {
    let mut map = Map::new();

    insert_filter_args(&mut map, &args.filters);
    // An omitted `--limit` stays absent so the tool applies find's default of 10
    // (matching the direct path's `build_find_query`).
    insert_paging(&mut map, &args.paging);
    insert_list(&mut map, "dynamic_keys", dynamic_keys);

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

/// Rebuild a [`RoutedFind`] from a `vault.find` `structuredContent` object.
///
/// The envelope (`total`/`returned`/`truncated`) rebuilds [`FindResult`]'s
/// counts; each `documents[i]` object is mapped back to a [`DocumentSummary`]
/// (plus a parallel [`DocumentDeep`] for the join-backed facets), keyed off the
/// same `--col`/`--all-cols` decision the direct
/// `find::query::select` makes — so `find::emit` renders the reconstruction
/// byte-identically to the direct path. Any shape mismatch is an `Err`, which the
/// caller maps to a verified direct open.
pub fn reconstruct(structured: &Value, args: &FindArgs) -> Result<RoutedFind> {
    let total = get_usize(structured, "vault.find", "total")?;
    let returned = get_usize(structured, "vault.find", "returned")?;
    let truncated = get_bool(structured, "vault.find", "truncated")?;
    // Required, not defaulted: the exit code derives from this bit, and guessing
    // it (e.g. an older daemon that predates the field) would silently break the
    // routed/direct exit-2 isomorphism — better to fall back to Direct.
    let has_diagnostic_errors = get_bool(structured, "vault.find", "has_diagnostic_errors")?;
    let documents = structured
        .get("documents")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "vault.find envelope: `documents` must be an array, got {}",
                json_type(structured.get("documents"))
            )
        })?;

    // Mirror `find::query::select`'s facet decisions (the shared predicates) so
    // the reconstructed deep vector is shaped exactly as the direct path's —
    // empty when the projection asks for no join-backed facet.
    let (facets, _fields) = split_cols(&args.col);
    let needs_deep = crate::find::query::needs_deep(&facets, args.all_cols);

    let mut matches = Vec::with_capacity(documents.len());
    let mut deep: Vec<Option<DocumentDeep>> = Vec::new();

    for doc in documents {
        let obj = doc.as_object().ok_or_else(|| {
            anyhow::anyhow!(
                "vault.find envelope: a `documents` entry must be an object, got {}",
                json_type(Some(doc))
            )
        })?;
        let path = Utf8PathBuf::from(obj.get("path").and_then(Value::as_str).ok_or_else(|| {
            anyhow::anyhow!(
                "vault.find envelope: a document's `path` must be a string, got {}",
                json_type(obj.get("path"))
            )
        })?);
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
        // The wire keys the absent-vs-null frontmatter distinction on KEY
        // presence (`doc_to_wire_json`, NRN-222): an absent key is a document
        // with no frontmatter block (`None`); `"frontmatter": null` is an
        // empty `---\n---` block (`Some(Value::Null)`), which the records
        // renderer prints as a `frontmatter  null` row.
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
    }

    Ok(RoutedFind {
        result: FindResult {
            matches,
            total,
            returned,
            truncated,
        },
        deep,
        has_diagnostic_errors,
    })
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

        let v = to_mcp_arguments(&args, &[]);
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
        // No dynamic keys → the private channel is omitted (byte-identical wire).
        assert!(v.get("dynamic_keys").is_none());
    }

    /// NRN-218: the desugared dynamic-field keys ride the private wire channel so
    /// the daemon can gate them; a canonical invocation omits the key entirely.
    #[test]
    fn to_mcp_arguments_carries_dynamic_keys() {
        let mut args = base_args();
        args.filters.eq = vec!["type:note".into()];
        let v = to_mcp_arguments(&args, &["type".to_string()]);
        assert_eq!(v["dynamic_keys"], json!(["type"]));
        assert_eq!(v["eq"], json!(["type:note"]));
    }

    // ── Round-trip isomorphism (NRN-222, per the count template) ──────────────
    //
    // The reconstruction is the exact inverse (for rendering purposes) of the
    // daemon's `vault.find` projection: build a `FindResult` + deep, project
    // it to the wire `FindOutput` the tool serializes, reconstruct, and assert the
    // RENDERED bytes match the direct path — across formats and `--col`. (Struct
    // equality is deliberately NOT asserted: fields the projection omits, e.g. an
    // unread `hash`/`body_text`, are reconstructed as defaults and never read.)

    /// Project a `FindResult` + deep to the `vault.find` wire envelope,
    /// exactly as `mcp::tools::find` does (`doc_to_json` per match + the count
    /// envelope), then serialize to the `structuredContent` JSON value.
    fn to_wire(result: &FindResult, deep: &[Option<DocumentDeep>], args: &FindArgs) -> Value {
        let documents: Vec<Value> = result
            .matches
            .iter()
            .enumerate()
            .map(|(i, d)| {
                // The REAL wire projection (what `vault.find` serializes),
                // including the absent-vs-null frontmatter distinction.
                crate::find::query::doc_to_wire_json(
                    d,
                    deep.get(i).and_then(|x| x.as_ref()),
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
        args: &FindArgs,
    ) -> (Vec<u8>, Vec<u8>) {
        let query = crate::find::query::build_find_query(args).unwrap();
        let format = args.format.unwrap_or(FindFormat::Records);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        crate::find::render::render(
            result,
            deep,
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
    fn assert_round_trip(result: FindResult, deep: Vec<Option<DocumentDeep>>, mut args: FindArgs) {
        for format in [
            FindFormat::Paths,
            FindFormat::Records,
            FindFormat::Json,
            FindFormat::Jsonl,
        ] {
            args.format = Some(format);
            let wire = to_wire(&result, &deep, &args);
            let routed = reconstruct(&wire, &args).unwrap();

            let (direct_out, direct_err) = render_bytes(&result, &deep, &args);
            let (routed_out, routed_err) = render_bytes(&routed.result, &routed.deep, &args);

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
        assert_round_trip(result, vec![], base_args());
    }

    /// A forwarded-note envelope (NRN-215): the daemon injects an
    /// `operator_notes` sibling into `structuredContent` under lock contention.
    /// `reconstruct` must ignore that extra key and rebuild the same result —
    /// routed stdout stays byte-identical while the note rides alongside for the
    /// routing seam (`route_read`) to re-emit on stderr.
    #[test]
    fn reconstruct_ignores_operator_notes_sibling() {
        let result = sample_result();
        let mut args = base_args();
        args.format = Some(FindFormat::Json);
        let mut wire = to_wire(&result, &[], &args);
        wire.as_object_mut().unwrap().insert(
            "operator_notes".into(),
            json!(["vault: another cache operation is in progress; using current cache state"]),
        );
        let routed = reconstruct(&wire, &args).unwrap();
        let (direct_out, direct_err) = render_bytes(&result, &[], &args);
        let (routed_out, routed_err) = render_bytes(&routed.result, &routed.deep, &args);
        assert_eq!(
            direct_out, routed_out,
            "stdout must ignore the notes sibling"
        );
        assert_eq!(
            direct_err, routed_err,
            "stderr must ignore the notes sibling"
        );
    }

    #[test]
    fn round_trip_bare_field_col() {
        let result = sample_result();
        let mut args = base_args();
        args.col = vec!["title".into()];
        assert_round_trip(result, vec![], args);
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
        assert_round_trip(result, vec![], args);
    }

    #[test]
    fn round_trip_body_and_stem_and_hash_facets() {
        let result = sample_result();
        let mut args = base_args();
        args.col = vec![".body".into(), ".stem".into(), ".document_hash".into()];
        assert_round_trip(result, vec![], args);
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
        assert_round_trip(result, deep, args);
    }

    #[test]
    fn round_trip_all_cols() {
        let result = sample_result();
        let deep = vec![Some(deep_for("note1.md")), Some(deep_for("note2.md"))];
        let mut args = base_args();
        args.all_cols = true;
        assert_round_trip(result, deep, args);
    }

    #[test]
    fn round_trip_empty_result() {
        let result = FindResult {
            matches: vec![],
            total: 0,
            returned: 0,
            truncated: false,
        };
        assert_round_trip(result, vec![], base_args());
    }

    /// A frontmatter-less document round-trips: the wire serializes its
    /// frontmatter as JSON `null`, which reconstruction must normalize back to
    /// `None` (mirroring show/route) — `Some(Value::Null)` would render a bogus
    /// `frontmatter  null` row under `--col .frontmatter` where the direct path
    /// prints "(no matching fields)".
    #[test]
    fn round_trip_no_frontmatter() {
        let bare = DocumentSummary {
            path: Utf8PathBuf::from("bare.md"),
            stem: "bare".into(),
            hash: "h".into(),
            frontmatter: None,
            body_text: "body\n".into(),
        };
        let result = FindResult {
            matches: vec![bare],
            total: 1,
            returned: 1,
            truncated: false,
        };
        // Both the default projection and the explicit `.frontmatter` facet.
        assert_round_trip(result.clone(), vec![], base_args());
        let mut args = base_args();
        args.col = vec![".frontmatter".into()];
        assert_round_trip(result, vec![], args);
    }

    /// An EMPTY `---\n---` frontmatter block is `Some(Value::Null)` on the
    /// direct path — a distinct state from a document with NO block (`None`).
    /// The wire keeps them apart (`"frontmatter": null` vs an absent key), and
    /// the round-trip must preserve the direct rendering: `--col .frontmatter
    /// --format records` prints a `frontmatter  null` row for the empty block,
    /// vs "(no matching fields)" for the absent one.
    #[test]
    fn round_trip_empty_frontmatter_block() {
        let empty_block = DocumentSummary {
            path: Utf8PathBuf::from("empty-block.md"),
            stem: "empty-block".into(),
            hash: "h".into(),
            frontmatter: Some(Value::Null),
            body_text: "body\n".into(),
        };
        let result = FindResult {
            matches: vec![empty_block],
            total: 1,
            returned: 1,
            truncated: false,
        };
        assert_round_trip(result.clone(), vec![], base_args());
        let mut args = base_args();
        args.col = vec![".frontmatter".into()];
        assert_round_trip(result, vec![], args);
    }

    // ── Exit-code isomorphism: the vault-diagnostics bit (NRN-222) ────────────

    /// The `has_diagnostic_errors` bit crosses the wire faithfully in both
    /// states — it is what `route_find` derives exit 2 vs 0 from.
    #[test]
    fn diagnostics_bit_round_trips() {
        let args = base_args();
        for bit in [false, true] {
            let mut wire = to_wire(&sample_result(), &[], &args);
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
        let mut wire = to_wire(&sample_result(), &[], &args);
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
        let scope = ctx.begin_request().unwrap();
        let out = crate::mcp::tools::find::handle(
            &ctx,
            &scope,
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
            crate::cache::command::open_for_query(&root, &crate::graph::IndexOptions::default(), false)
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
