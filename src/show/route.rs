//! CLI→service routing translation for `norn get` (NRN-222).
//!
//! `get` is routable byte-identically because the `vault.get` MCP tool's
//! [`GetOutput`](crate::mcp::tools::get::GetOutput) ships each resolved document
//! as the FULL serialized [`ShowRecord`] (the tool's `col` opts facets IN but
//! does NOT narrow — NRN-173), plus the run's `notes` (NRN-214). The client
//! rebuilds a [`ShowReport`] from that payload and renders it through the SAME
//! `show::render::*_with_col` seams the direct path uses, applying the CLI's
//! client-side `--col` narrowing itself — so routed and direct output are
//! byte-for-byte equal.
//!
//! **Gated to Direct (handled by the caller, `route_get` in `src/lib.rs`):**
//! `--format markdown` (a byte-faithful disk read with bespoke multi-doc
//! handling) and `--section` (the wire serializes sections as an
//! alphabetically-keyed object, dropping the request order the `records`
//! renderer needs). Everything else routes.
//!
//! Both functions here are pure so they unit-test without a live daemon; the
//! probe + wire round-trip live in the routing seam (`src/lib.rs`).

use anyhow::Result;
use camino::Utf8PathBuf;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::cli::GetArgs;
use crate::show::{ShowRecord, ShowReport};

/// Translate parsed `norn get` args into the `vault.get` tool's parameter object
/// (the `GetParams` shape in `src/mcp/tools/get.rs`).
///
/// `--format` is CLI-only (the client renders the returned records). `--section`
/// is never sent — the caller gates a `--section` invocation to Direct. `col` is
/// sent so the daemon LOADS the on-request facets (`.body`/`.raw`/`.document_hash`);
/// the actual `--col` narrowing is applied client-side by the renderer, so the
/// tool's non-narrowing `col` semantics don't affect the routed output.
pub fn to_mcp_arguments(args: &GetArgs) -> Value {
    let mut map = Map::new();
    map.insert(
        "targets".into(),
        Value::Array(args.targets.iter().cloned().map(Value::String).collect()),
    );

    // `col` is a comma-joined token on the tool side (it re-splits); the CLI's
    // `value_delimiter = ','` already split it into a Vec, so join it back.
    if !args.col.is_empty() {
        map.insert("col".into(), Value::String(args.col.join(",")));
    }
    if args.all_cols {
        map.insert("all_cols".into(), Value::Bool(true));
    }

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
    // starts_at defaults to 1 on both surfaces; send only when non-default.
    if p.starts_at != 1 {
        map.insert("starts_at".into(), Value::Number(p.starts_at.into()));
    }

    Value::Object(map)
}

/// Rebuild a [`ShowReport`] from a `vault.get` `structuredContent` object.
///
/// Each `records[i]` is the full serialized `ShowRecord`, mapped back field for
/// field (`stem`, absent from the wire as `#[serde(skip)]`, is re-derived from
/// the path exactly as `graph::build` computes it). `notes` travels verbatim so
/// the CLI's stderr diagnostics and exit-1 signal (`ShowReport::has_error`) are
/// reproduced. `sections`/`section_failures` are always empty here: a
/// `--section` invocation is gated to Direct by the caller. Any shape mismatch is
/// an `Err`, which the caller maps to a verified direct open.
pub fn reconstruct(structured: &Value, _args: &GetArgs) -> Result<ShowReport> {
    let records = structured
        .get("records")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("get envelope missing `records` array: {structured}"))?
        .iter()
        .map(record_from_wire)
        .collect::<Result<Vec<_>>>()?;

    let notes = structured
        .get("notes")
        .map(|v| serde_json::from_value::<Vec<String>>(v.clone()))
        .transpose()?
        .unwrap_or_default();

    Ok(ShowReport {
        records,
        notes,
        // `--section` is gated to Direct, so the daemon never resolves sections
        // and this list is always empty on a routed get.
        section_failures: Vec::new(),
    })
}

fn record_from_wire(v: &Value) -> Result<ShowRecord> {
    let obj = v
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("get record is not an object: {v}"))?;
    let path = Utf8PathBuf::from(
        obj.get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("get record missing string `path`: {v}"))?,
    );
    // `stem` is `#[serde(skip)]` on the wire but a pure function of the path;
    // recompute it as `graph::build` does so `--col .stem` renders identically.
    let stem = path.file_stem().unwrap_or_default().to_string();

    Ok(ShowRecord {
        path,
        stem,
        document_hash: obj
            .get("document_hash")
            .and_then(Value::as_str)
            .map(str::to_string),
        // `None` frontmatter serializes as JSON `null`; normalize it back so the
        // re-serialized record and the record renderers see the same absence.
        frontmatter: obj.get("frontmatter").filter(|v| !v.is_null()).cloned(),
        headings: take_vec(obj, "headings")?,
        outgoing_links: take_vec(obj, "outgoing_links")?,
        unresolved_links: take_vec(obj, "unresolved_links")?,
        incoming_links: take_vec(obj, "incoming_links")?,
        body: obj.get("body").and_then(Value::as_str).map(str::to_string),
        // Never routed: `--section` is gated to Direct.
        sections: None,
        raw: obj.get("raw").and_then(Value::as_str).map(str::to_string),
    })
}

/// Deserialize `obj[key]` into `Vec<T>`, treating an absent key as an empty vec.
fn take_vec<T: DeserializeOwned>(obj: &Map<String, Value>, key: &str) -> Result<Vec<T>> {
    match obj.get(key) {
        Some(value) => Ok(serde_json::from_value(value.clone())?),
        None => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{GetFormat, SortPaginateArgs};
    use crate::core::{Heading, Link, LinkKind, LinkStatus};
    use serde_json::json;

    fn base_args() -> GetArgs {
        GetArgs {
            targets: vec!["note".into()],
            paging: SortPaginateArgs {
                sort: None,
                desc: false,
                limit: None,
                no_limit: false,
                starts_at: 1,
            },
            all_cols: false,
            col: vec![],
            section: vec![],
            format: GetFormat::Records,
        }
    }

    #[test]
    fn to_mcp_arguments_joins_col_and_maps_paging() {
        let mut args = base_args();
        args.targets = vec!["a".into(), "b".into()];
        args.col = vec!["title".into(), ".body".into()];
        args.paging.sort = Some("path".into());
        args.paging.limit = Some(3);

        let v = to_mcp_arguments(&args);
        assert_eq!(v["targets"], json!(["a", "b"]));
        assert_eq!(v["col"], "title,.body");
        assert_eq!(v["sort"], "path");
        assert_eq!(v["limit"], 3);
        assert!(v.get("all_cols").is_none());
        assert!(v.get("starts_at").is_none());
    }

    // ── Round-trip isomorphism (NRN-222, per the count template) ──────────────

    /// Project a `ShowReport` to the `vault.get` wire envelope, exactly as
    /// `mcp::tools::get::GetOutput::from_report` does (each record serialized in
    /// full, plus notes), then serialize to the `structuredContent` value.
    fn to_wire(report: &ShowReport) -> Value {
        let records: Vec<Value> = report
            .records
            .iter()
            .map(|r| serde_json::to_value(r).unwrap())
            .collect();
        json!({
            "records": records,
            "section_failures": [],
            "notes": report.notes,
        })
    }

    /// Assert routed (reconstructed) render bytes equal the direct render bytes
    /// for every non-markdown format, under the given `--col` projection.
    fn assert_round_trip(report: ShowReport, cols: Vec<String>) {
        let wire = to_wire(&report);
        let mut args = base_args();
        args.col = cols.clone();
        let routed = reconstruct(&wire, &args).unwrap();

        use crate::show::render::{
            render_json_with_col, render_jsonl_with_col, render_paths, render_records_with_col,
        };
        assert_eq!(
            render_json_with_col(&report, &cols),
            render_json_with_col(&routed, &cols),
            "json must match (col={cols:?})"
        );
        assert_eq!(
            render_jsonl_with_col(&report, &cols),
            render_jsonl_with_col(&routed, &cols),
            "jsonl must match (col={cols:?})"
        );
        assert_eq!(
            render_paths(&report),
            render_paths(&routed),
            "paths must match (col={cols:?})"
        );
        assert_eq!(
            render_records_with_col(&report, &cols),
            render_records_with_col(&routed, &cols),
            "records must match (col={cols:?})"
        );
    }

    fn record(path: &str, frontmatter: Option<Value>, body: Option<&str>) -> ShowRecord {
        let path = Utf8PathBuf::from(path);
        let link = Link {
            source_path: path.clone(),
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
        ShowRecord {
            stem: path.file_stem().unwrap_or_default().to_string(),
            document_hash: Some("deadbeef".into()),
            frontmatter,
            headings: vec![Heading {
                level: 2,
                text: "Section".into(),
                slug: "section".into(),
                source_span: None,
            }],
            outgoing_links: vec![link],
            unresolved_links: vec![],
            incoming_links: vec![],
            body: body.map(str::to_string),
            sections: None,
            raw: None,
            path,
        }
    }

    fn sample_report() -> ShowReport {
        ShowReport {
            records: vec![
                record(
                    "note.md",
                    Some(json!({"type": "note", "title": "Hello", "status": "active"})),
                    Some("The body\n"),
                ),
                record("other.md", Some(json!({"type": "task"})), None),
            ],
            notes: vec!["note: 'x' resolved to 2 docs".into()],
            section_failures: vec![],
        }
    }

    #[test]
    fn round_trip_default() {
        assert_round_trip(sample_report(), vec![]);
    }

    #[test]
    fn round_trip_bare_field_col() {
        assert_round_trip(sample_report(), vec!["title".into()]);
    }

    #[test]
    fn round_trip_facet_cols() {
        assert_round_trip(
            sample_report(),
            vec![
                ".stem".into(),
                ".document_hash".into(),
                ".headings".into(),
                ".outgoing_links".into(),
                ".body".into(),
            ],
        );
    }

    #[test]
    fn round_trip_no_frontmatter() {
        let report = ShowReport {
            records: vec![record("bare.md", None, None)],
            notes: vec![],
            section_failures: vec![],
        };
        assert_round_trip(report, vec![]);
    }

    #[test]
    fn round_trip_raw_facet() {
        let mut report = sample_report();
        report.records[0].raw = Some("---\ntype: note\n---\nThe body\n".into());
        assert_round_trip(report, vec![".raw".into()]);
    }

    #[test]
    fn notes_survive_reconstruction() {
        let wire = to_wire(&sample_report());
        let routed = reconstruct(&wire, &base_args()).unwrap();
        assert_eq!(
            routed.notes,
            vec!["note: 'x' resolved to 2 docs".to_string()]
        );
        assert!(!routed.has_error());
    }
}
