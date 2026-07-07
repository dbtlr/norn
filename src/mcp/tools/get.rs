//! `vault.get` — structured document fetch.
//!
//! The pure handler reuses [`crate::show::run`], the exact code path behind
//! `norn get`. It returns the same [`ShowReport`] struct the CLI renders, so the
//! MCP surface and the CLI can never drift on resolution, link projection, or
//! facet selection.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cli::{GetArgs, GetFormat, SortPaginateArgs};
use crate::mcp::context::VaultContext;
use crate::show::ShowReport;

/// Default for [`GetParams::starts_at`] — the CLI's `--starts-at` default (1).
/// serde's numeric default is 0; get's paging is 1-indexed, so the absent-field
/// value must be 1 to page identically to `norn get`.
fn default_starts_at() -> usize {
    1
}

/// Parameters for `vault.get`.
///
/// Mirrors `norn get`'s daily surface: one or more targets, an optional column
/// request, the shared sort/paging knobs ([`SortPaginateArgs`]), the repeatable
/// `--section` heading slice, and `--all-cols`. The byte-faithful `markdown`
/// output format stays CLI-only (a rendering concern; the MCP envelope is always
/// structured JSON).
///
/// The sort/paging fields carry the SAME names and defaults as the CLI's
/// `SortPaginateArgs` (NRN-173 parity), so an MCP client pages a resolved record
/// set exactly as the CLI does.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct GetParams {
    /// One or more document targets (stem or path), as `norn get` accepts.
    pub targets: Vec<String>,
    /// Optional column request, comma-separated, in `norn get --col` syntax (bare
    /// frontmatter fields like `status,title`; dot-prefixed facets like `.body`,
    /// `.headings`). NOTE (v1): this only controls whether the on-request facets
    /// (`.body`, `.raw`, `.document_hash` — the full-content blake3 the CAS uses)
    /// are *included* — it does NOT narrow the payload. Every record always
    /// ships its full structured shape (dump-everything default); bare-field /
    /// facet narrowing is not applied to the MCP envelope. (This `col` SEMANTICS
    /// divergence from the CLI — which narrows — is a known, tracked gap
    /// deferred to the NRN-185/190 break window; it is deliberately NOT changed
    /// here.)
    #[serde(default)]
    pub col: Option<String>,

    /// Sort by field (frontmatter key, `path`, or `stem`); ascending by default.
    /// Mirrors `norn get --sort`. Absent → resolution order.
    #[serde(default)]
    pub sort: Option<String>,
    /// Sort descending (only meaningful with `sort`). Mirrors `--desc`.
    #[serde(default)]
    pub desc: bool,
    /// Maximum number of records to return. Absent → every named target (get's
    /// default; unlike `find`'s 10). Mirrors `--limit`.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Return all records, ignoring `limit`. Mirrors `--no-limit`.
    #[serde(default)]
    pub no_limit: bool,
    /// 1-indexed starting offset for paging. Defaults to 1. Mirrors `--starts-at`.
    #[serde(default = "default_starts_at")]
    pub starts_at: usize,

    /// Named sections to read, by exact heading text. Repeatable — one whole
    /// heading string per entry, so a heading containing a comma is addressable
    /// verbatim (exactly `norn get --section`'s one-string-per-heading
    /// semantics). Each resolved heading appears in the record's `sections`
    /// object (`{heading: content}`), byte-identical to `norn get --format json`.
    /// A heading missing or ambiguous in a document is warn-and-omitted for that
    /// document (siblings and other targets are unaffected); if NONE of the
    /// requested headings resolve for a target, that target hard-fails and the
    /// whole call returns an error, mirroring the CLI's nonzero-exit contract.
    #[serde(default)]
    pub section: Vec<String>,

    /// Emit the full structured dump for each record: every cache-served facet,
    /// including `.body`. Mirrors `norn get --all-cols`. NOTE: the MCP envelope
    /// already serializes each record's full structured shape, so relative to
    /// the default this only additionally loads the `body` field; accepting it
    /// explicitly is the parity contract (NRN-173).
    #[serde(default)]
    pub all_cols: bool,
}

/// Structured output for `vault.get`.
///
/// rmcp requires a tool's advertised `outputSchema` to have a root `type:
/// object` (a bare `serde_json::Value` schema is untyped and is rejected at
/// server startup). We get that root-object schema from this typed envelope
/// while deliberately keeping the per-record payload as `serde_json::Value`:
/// the alternative — deriving `schemars::JsonSchema` on [`ShowRecord`] and its
/// ~8 transitive `pub(crate)` core types (`Heading`, `Link`, `LinkStatus`, …,
/// all carrying `camino::Utf8PathBuf`, which has no `JsonSchema` impl) — is a
/// large, drift-prone change to shared core types for no client-visible gain in
/// v1. The full record structure still travels faithfully in the JSON; only the
/// inner record *schema* is left generic. Later read tools copy this envelope
/// shape.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct GetOutput {
    /// One entry per resolved document, in resolution order. Each is the JSON
    /// form of a `norn get` record: `path`, `frontmatter`, `headings`, the three
    /// link sets, and (when the matching col was requested) `body` / `raw` /
    /// `document_hash`.
    pub records: Vec<serde_json::Value>,
}

impl GetOutput {
    /// Convert a [`ShowReport`] into the MCP output envelope. The report's
    /// `notes` (ambiguous-stem / missing-target diagnostics) are CLI-stderr
    /// concerns and `#[serde(skip)]` on the report, so they are not surfaced
    /// here; a missing target simply yields zero records.
    fn from_report(report: &ShowReport) -> Result<Self> {
        let records = report
            .records
            .iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Self { records })
    }
}

/// Build the MCP output envelope for `vault.get`: run the pure handler, then
/// project the report into the typed [`GetOutput`]. This is the single function
/// the `#[tool]` wrapper calls.
pub fn handle_output(ctx: &VaultContext, p: GetParams) -> Result<GetOutput> {
    let report = handle(ctx, p)?;
    GetOutput::from_report(&report)
}

/// Pure handler for `vault.get`. Opens a fresh query cache (per-call freshness),
/// constructs [`GetArgs`] with `norn get`'s defaults, and runs the show path.
pub fn handle(ctx: &VaultContext, p: GetParams) -> Result<ShowReport> {
    let cache = ctx.query_cache()?;

    // Mirror `norn get --col`'s parsing: the CLI uses `value_delimiter = ','`, so
    // each comma-separated token becomes one element of the `Vec<String>` that
    // `split_cols` later splits into facets vs. bare fields. We additionally trim
    // each token and drop empties — a forgiving superset of the CLI (clap does
    // not trim, but a space-padded facet name like " .body" never matches a known
    // facet anyway, so trimming only ever helps). An empty / whitespace-only
    // string yields no columns (full structured dump), matching an absent --col.
    let col: Vec<String> = match p.col {
        Some(s) => s
            .split(',')
            .map(str::trim)
            .filter(|tok| !tok.is_empty())
            .map(str::to_string)
            .collect(),
        None => Vec::new(),
    };

    let args = GetArgs {
        targets: p.targets,
        // Sort/paging carry the MCP params straight through into the shared
        // `SortPaginateArgs` (NRN-173). Names + defaults match the CLI, so MCP
        // and CLI sort/page identically over the resolved record set.
        paging: SortPaginateArgs {
            sort: p.sort,
            desc: p.desc,
            limit: p.limit,
            no_limit: p.no_limit,
            starts_at: p.starts_at,
        },
        all_cols: p.all_cols,
        col,
        section: p.section,
        // Records is `norn get`'s default format and the one `show::run` uses to
        // decide the `--section` slice is CONSUMED (Json/Jsonl/Records consume
        // it; Paths/Markdown ignore it). The MCP wrapper serializes the returned
        // `ShowReport` to JSON regardless, so this governs which facets
        // `show::run` loads (body, sections), not a textual rendering.
        format: GetFormat::Records,
    };

    let report = crate::show::run(&cache, &args)?;

    // Section hard-fail signal (NRN-173, mirroring the CLI's exit-1 contract):
    // when `--section` is requested and NONE of the requested headings resolved
    // for a target, `show::run` pushes an `error:` note naming that target (and
    // the CLI exits 1). The MCP surface has no per-record status field, so the
    // faithful analogue is a call-level error carrying those notes. Only the
    // section-specific error is escalated — a plain missing target (no sections)
    // still yields zero records without erroring (NRN-183 exit-signal asymmetry
    // is tracked separately and unchanged here).
    if !args.section.is_empty() {
        let hard_fails: Vec<&str> = report
            .notes
            .iter()
            .filter(|n| n.starts_with("error:") && n.contains("--section"))
            .map(String::as_str)
            .collect();
        if !hard_fails.is_empty() {
            anyhow::bail!("{}", hard_fails.join("; "));
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    /// Seed a temp vault with a single `note.md` carrying frontmatter.
    fn seeded_vault() -> (TempDir, Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-get-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(
            root.join("note.md"),
            "---\ntype: note\ntitle: Hello Note\nstatus: active\n---\nNote body\n",
        )
        .unwrap();
        (tmp, root)
    }

    #[test]
    fn handle_returns_single_record_for_seeded_doc() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            GetParams {
                targets: vec!["note".into()],
                col: None,
                ..Default::default()
            },
        )
        .expect("handle should succeed");

        assert_eq!(
            report.records.len(),
            1,
            "expected exactly one record, got {}: notes={:?}",
            report.records.len(),
            report.notes
        );

        let rec = &report.records[0];
        assert!(
            rec.path.as_str().ends_with("note.md"),
            "record path should end with note.md, got {}",
            rec.path
        );

        let fm = rec
            .frontmatter
            .as_ref()
            .expect("seeded doc has frontmatter");
        assert_eq!(
            fm.get("type").and_then(|v| v.as_str()),
            Some("note"),
            "frontmatter type should reflect the seeded field, got {fm:?}"
        );
        assert_eq!(
            fm.get("title").and_then(|v| v.as_str()),
            Some("Hello Note"),
            "frontmatter title should reflect the seeded field, got {fm:?}"
        );
        assert_eq!(
            fm.get("status").and_then(|v| v.as_str()),
            Some("active"),
            "frontmatter status should reflect the seeded field, got {fm:?}"
        );
    }

    /// NRN-105: `vault.get` with `col: ".document_hash"` surfaces the
    /// full-content blake3 in the serialized MCP envelope (the record is
    /// serialized directly, no facet narrowing) — how an off-filesystem MCP
    /// client reads the hash to feed `vault.edit`'s `expected_hash`. Absent
    /// without the col, so the default envelope stays byte-identical.
    #[test]
    fn handle_document_hash_facet_surfaces_in_envelope() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let expected = blake3::hash(&std::fs::read(root.join("note.md")).unwrap())
            .to_hex()
            .to_string();

        // Requested → present in the serialized record.
        let report = handle(
            &ctx,
            GetParams {
                targets: vec!["note".into()],
                col: Some(".document_hash".into()),
                ..Default::default()
            },
        )
        .expect("handle should succeed");
        let json = serde_json::to_value(&report.records[0]).unwrap();
        assert_eq!(
            json.get("document_hash").and_then(|v| v.as_str()),
            Some(expected.as_str()),
            "MCP envelope must carry document_hash when requested: {json}"
        );

        // Not requested → absent (default envelope unchanged).
        let plain = handle(
            &ctx,
            GetParams {
                targets: vec!["note".into()],
                col: None,
                ..Default::default()
            },
        )
        .expect("handle should succeed");
        let pjson = serde_json::to_value(&plain.records[0]).unwrap();
        assert!(
            pjson.get("document_hash").is_none(),
            "default envelope must omit document_hash: {pjson}"
        );
    }

    #[test]
    fn handle_col_projection_parses_like_cli() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        // `.body` facet must load the body, mirroring `norn get --col .body`.
        let report = handle(
            &ctx,
            GetParams {
                targets: vec!["note".into()],
                col: Some(".body".into()),
                ..Default::default()
            },
        )
        .expect("handle should succeed");

        assert_eq!(report.records.len(), 1);
        assert!(
            report.records[0].body.is_some(),
            "`.body` col should have loaded the document body"
        );
    }

    #[test]
    fn handle_missing_target_yields_zero_records_with_note() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            GetParams {
                targets: vec!["does-not-exist".into()],
                col: None,
                ..Default::default()
            },
        )
        .expect("handle should succeed even when the target is missing");

        assert!(report.records.is_empty(), "no record for a missing target");
        assert!(
            report.notes.iter().any(|n| n.starts_with("error:")),
            "a missing target should produce an error note, got {:?}",
            report.notes
        );
    }

    // ── NRN-173: sort / paging / section / all_cols parity ──────────────────

    /// Seed a vault with a doc carrying two sections and N frontmatter-ordered
    /// docs for sort/paging coverage.
    fn sectioned_vault() -> (TempDir, Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-get-sec-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(
            root.join("doc.md"),
            "---\ntype: note\n---\n## Task Description\ndo the thing\n## Annotations\nnote here\n",
        )
        .unwrap();
        std::fs::write(root.join("a.md"), "---\norder: '2'\n---\nA\n").unwrap();
        std::fs::write(root.join("b.md"), "---\norder: '1'\n---\nB\n").unwrap();
        std::fs::write(root.join("c.md"), "---\norder: '3'\n---\nC\n").unwrap();
        (tmp, root)
    }

    /// The `sections` object an MCP `vault.get` returns must be byte-for-byte the
    /// object `norn get --format json` emits — same slice seam (`show::run`),
    /// same keyed shape.
    #[test]
    fn section_object_matches_cli_json_slice() {
        let (_tmp, root) = sectioned_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            GetParams {
                targets: vec!["doc".into()],
                section: vec!["Task Description".into()],
                ..Default::default()
            },
        )
        .expect("handle should succeed");
        let mcp_sections = serde_json::to_value(&report.records[0]).unwrap()["sections"].clone();

        // Byte content: heading line through end-of-section, verbatim.
        assert_eq!(
            mcp_sections["Task Description"].as_str().unwrap(),
            "## Task Description\ndo the thing\n"
        );

        // Build the CLI seam directly (fresh cache) and render --format json, then
        // compare the sections object — proves MCP == CLI at the object level.
        let cache = ctx.query_cache().unwrap();
        let cli_args = GetArgs {
            targets: vec!["doc".into()],
            paging: SortPaginateArgs {
                sort: None,
                desc: false,
                limit: None,
                no_limit: false,
                starts_at: 1,
            },
            all_cols: false,
            col: vec![],
            section: vec!["Task Description".into()],
            format: crate::cli::GetFormat::Json,
        };
        let cli_report = crate::show::run(&cache, &cli_args).unwrap();
        let cli_json: serde_json::Value =
            serde_json::from_str(&crate::show::render::render_json(&cli_report)).unwrap();
        assert_eq!(
            mcp_sections, cli_json[0]["sections"],
            "MCP sections object must equal the CLI --format json sections object"
        );
    }

    /// A heading missing (or ambiguous) in a doc is warn-and-omitted for that
    /// doc; sibling headings still resolve and the call succeeds.
    #[test]
    fn section_partial_miss_warns_and_omits_but_succeeds() {
        let (_tmp, root) = sectioned_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            GetParams {
                targets: vec!["doc".into()],
                section: vec!["Nope".into(), "Annotations".into()],
                ..Default::default()
            },
        )
        .expect("partial resolution is not a hard failure");
        let sections = serde_json::to_value(&report.records[0]).unwrap()["sections"].clone();
        assert!(
            sections.get("Annotations").is_some(),
            "the resolvable heading is present: {sections}"
        );
        assert!(
            sections.get("Nope").is_none(),
            "the missing heading is omitted: {sections}"
        );
    }

    /// When NONE of the requested headings resolve for a target, the call hard-
    /// fails (mirrors the CLI exit-1 contract), surfaced as an MCP error.
    #[test]
    fn section_all_missing_hard_fails() {
        let (_tmp, root) = sectioned_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let result = handle(
            &ctx,
            GetParams {
                targets: vec!["doc".into()],
                section: vec!["Nope".into()],
                ..Default::default()
            },
        );
        assert!(
            result.is_err(),
            "all-missing sections must hard-fail, got {result:?}"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("none of the requested") && err.contains("--section"),
            "error must name the section hard-fail, got: {err}"
        );
    }

    /// Sort + desc order records; mirrors `norn get --sort order --desc`.
    #[test]
    fn sort_desc_orders_records() {
        let (_tmp, root) = sectioned_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            GetParams {
                targets: vec!["a".into(), "b".into(), "c".into()],
                sort: Some("order".into()),
                desc: true,
                ..Default::default()
            },
        )
        .expect("handle should succeed");
        let paths: Vec<&str> = report.records.iter().map(|r| r.path.as_str()).collect();
        // order values: a=2, b=1, c=3 → desc → c(3), a(2), b(1)
        assert_eq!(paths, vec!["c.md", "a.md", "b.md"]);
    }

    /// `starts_at` + `limit` page the resolved record set; mirrors the CLI.
    #[test]
    fn limit_and_starts_at_page_records() {
        let (_tmp, root) = sectioned_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            GetParams {
                targets: vec!["a".into(), "b".into(), "c".into()],
                sort: Some("order".into()),
                starts_at: 2,
                limit: Some(1),
                ..Default::default()
            },
        )
        .expect("handle should succeed");
        // sorted asc by order: b(1), a(2), c(3); starts_at=2 → a,c; limit 1 → a
        let paths: Vec<&str> = report.records.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["a.md"]);
    }

    /// `all_cols: true` loads the body field, mirroring `norn get --all-cols`.
    #[test]
    fn all_cols_loads_body() {
        let (_tmp, root) = sectioned_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let with = handle(
            &ctx,
            GetParams {
                targets: vec!["doc".into()],
                all_cols: true,
                ..Default::default()
            },
        )
        .expect("handle should succeed");
        assert!(
            with.records[0].body.is_some(),
            "all_cols must load the body"
        );

        let without = handle(
            &ctx,
            GetParams {
                targets: vec!["doc".into()],
                ..Default::default()
            },
        )
        .expect("handle should succeed");
        assert!(
            without.records[0].body.is_none(),
            "default (no all_cols) omits the body"
        );
    }

    /// The default (absent) `starts_at` deserializes to 1, not serde's 0, so an
    /// MCP call with no paging args pages identically to `norn get`.
    #[test]
    fn starts_at_defaults_to_one() {
        let p: GetParams = serde_json::from_value(serde_json::json!({
            "targets": ["doc"]
        }))
        .expect("deserialize");
        assert_eq!(p.starts_at, 1, "absent starts_at must default to 1");
    }
}
