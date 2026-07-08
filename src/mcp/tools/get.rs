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
use crate::mcp::mutation_result::MutationResult;
use crate::show::{SectionFailure, ShowReport};

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
    /// Mirrors `--no-limit`. NOTE: on the get path `limit` already defaults to
    /// `None` (return every named target — get's default, unlike `find`'s 10),
    /// so `no_limit` alone is ALREADY the default behavior and is effectively a
    /// no-op; the get paging path does not read it. It is mutually exclusive
    /// with `limit`: setting both is a params error (mirroring the CLI's clap
    /// `conflicts_with`), refused before any work rather than silently
    /// truncating.
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
    /// requested headings resolve for a target, that target is reported in the
    /// output's `section_failures` list (the additive structured mapping of the
    /// CLI's nonzero-exit contract) — the call does NOT fail as a whole, so every
    /// good target's records (and the all-missing target's own record, with an
    /// empty `sections`) are still returned.
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
    /// Targets for which `--section` was requested but NONE of the requested
    /// headings resolved (all missing/ambiguous). This is the additive
    /// structured MCP mapping of the CLI's section exit-1: rather than failing
    /// the whole call (which would discard every good target's records), each
    /// all-missing target is reported here (with its requested headings) while
    /// its own record — carrying an empty `sections` — and every sibling
    /// target's records still ship in `records`. Empty when no target
    /// all-missed. Never parsed from note text; sourced from
    /// [`ShowReport::section_failures`].
    pub section_failures: Vec<SectionFailure>,
    /// Non-fatal diagnostics from the get run: ambiguous-stem warnings and
    /// `error:`-prefixed missing-target / all-missed-section messages (NRN-214).
    /// These are the CLI's stderr notes — an off-filesystem MCP consumer could
    /// not otherwise see them, and an `error:` note is exactly the CLI's exit-1
    /// signal (which this tool also maps to `isError: true`). A consumer keys on
    /// the `error:` prefix, or on the structured `section_failures` for the
    /// section case.
    pub notes: Vec<String>,
}

impl GetOutput {
    /// Convert a [`ShowReport`] into the MCP output envelope. Carries the report's
    /// `notes` (which are `#[serde(skip)]` on the report so CLI `--format json`
    /// stays byte-identical) so an MCP consumer sees the same diagnostics the CLI
    /// writes to stderr, including the `error:`-prefixed not-found signal.
    fn from_report(report: &ShowReport) -> Result<Self> {
        let records = report
            .records
            .iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Self {
            records,
            section_failures: report.section_failures.clone(),
            notes: report.notes.clone(),
        })
    }
}

/// Build the MCP output envelope for `vault.get`: run the pure handler, project
/// the report into the typed [`GetOutput`], and wrap it with the `isError` bit
/// derived from the report's `error:` notes (NRN-214) — a requested target that
/// did not resolve (or an all-missed `--section`) maps to `isError: true` while
/// still returning every good target's records + the diagnostics. This is the
/// single function the `#[tool]` wrapper calls.
pub fn handle_output(ctx: &VaultContext, p: GetParams) -> Result<MutationResult<GetOutput>> {
    let report = handle(ctx, p)?;
    let is_error = report.has_error();
    let output = GetOutput::from_report(&report)?;
    Ok(MutationResult::from_flag(output, is_error))
}

/// Pure handler for `vault.get`. Opens a fresh query cache (per-call freshness),
/// constructs [`GetArgs`] with `norn get`'s defaults, and runs the show path.
pub fn handle(ctx: &VaultContext, p: GetParams) -> Result<ShowReport> {
    // Mirror the CLI's `--limit` / `--no-limit` clap `conflicts_with` (F3): the
    // two are mutually exclusive. On the get path `limit` defaults to None, so
    // `no_limit` alone is already the default (return every target) and the
    // paging path never reads it — but accepting BOTH would silently truncate
    // where the CLI hard-errors, so refuse up front, before any work.
    if p.limit.is_some() && p.no_limit {
        anyhow::bail!("--limit and --no-limit are mutually exclusive");
    }

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

    // Section hard-fail signal (F1/F2, mirroring the CLI's exit-1 contract):
    // when `--section` is requested and NONE of the requested headings resolve
    // for a target, `show::run` records it in `ShowReport::section_failures`
    // (the structured twin of the CLI's `error:` note). The MCP surface maps
    // that list to `GetOutput::section_failures` — an ADDITIVE field — rather
    // than bailing the whole call: a batch keeps every good target's records
    // even when one sibling all-misses, and there is no note-string parsing
    // anywhere (which previously false-positived on a plain unresolved target
    // whose name contained `--section`). A plain missing target (no sections)
    // still yields zero records without erroring (NRN-183 exit-signal asymmetry
    // is tracked separately and unchanged here).
    crate::show::run(&cache, &args)
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

    /// When NONE of the requested headings resolve for a target, the target is
    /// reported in the additive `section_failures` list (the structured MCP
    /// mapping of the CLI's exit-1) — the call does NOT fail as a whole, and the
    /// all-missing target's own record still ships with an empty `sections`.
    #[test]
    fn section_all_missing_reported_structurally_not_bailed() {
        let (_tmp, root) = sectioned_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let out = handle_output(
            &ctx,
            GetParams {
                targets: vec!["doc".into()],
                section: vec!["Nope".into()],
                ..Default::default()
            },
        )
        .expect("all-missing sections are reported structurally, not bailed");
        let out = out.value();

        // The all-missing target is reported in section_failures with its headings.
        assert_eq!(out.section_failures.len(), 1, "one all-missing target");
        assert!(
            out.section_failures[0].path.ends_with("doc.md"),
            "section failure names the target path: {:?}",
            out.section_failures[0]
        );
        assert_eq!(
            out.section_failures[0].requested_headings,
            vec!["Nope".to_string()]
        );
        // Its record still ships, with an empty sections object.
        assert_eq!(out.records.len(), 1, "the record is still returned");
        assert_eq!(
            out.records[0]["sections"],
            serde_json::json!({}),
            "all-missing target carries an empty sections object"
        );
    }

    /// F4: a multi-target batch where one target resolves a heading and a
    /// sibling all-misses — the good target's sections survive intact, the
    /// all-missing target is isolated into `section_failures`, and the call
    /// succeeds structurally (no whole-call bail losing the good target's data).
    #[test]
    fn section_multi_target_isolates_failure_keeps_good_target() {
        let (_tmp, root) = sectioned_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        // doc.md has "## Task Description"; a.md has no such heading.
        let out = handle_output(
            &ctx,
            GetParams {
                targets: vec!["doc".into(), "a".into()],
                section: vec!["Task Description".into()],
                ..Default::default()
            },
        )
        .expect("a multi-target section batch must succeed structurally");
        let out = out.value();

        // Both records ship (good target + all-missing target).
        assert_eq!(out.records.len(), 2, "both targets return a record");
        let doc_rec = out
            .records
            .iter()
            .find(|r| r["path"].as_str().unwrap().ends_with("doc.md"))
            .expect("good target present");
        assert_eq!(
            doc_rec["sections"]["Task Description"].as_str().unwrap(),
            "## Task Description\ndo the thing\n",
            "the good target keeps its resolved section intact"
        );

        // The all-missing sibling is isolated into section_failures.
        assert_eq!(out.section_failures.len(), 1, "only the sibling all-missed");
        assert!(
            out.section_failures[0].path.ends_with("a.md"),
            "the section failure names the all-missing sibling: {:?}",
            out.section_failures[0]
        );
        assert_eq!(
            out.section_failures[0].requested_headings,
            vec!["Task Description".to_string()]
        );
    }

    /// F4: false-positive guard — a target whose STEM contains the substring
    /// `--section` but does NOT resolve to any doc must be treated as a plain
    /// missing target, NOT a section failure. The old note-substring guard
    /// (`contains("--section")`) false-positived here and bailed the whole call.
    #[test]
    fn section_missing_target_named_like_section_is_not_a_section_failure() {
        let (_tmp, root) = sectioned_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let out = handle_output(
            &ctx,
            GetParams {
                // Stem embeds "--section"; does not resolve to any doc.
                targets: vec!["my--section-doc".into()],
                section: vec!["Whatever".into()],
                ..Default::default()
            },
        )
        .expect("a non-resolving target must not be treated as a section failure");
        let out = out.value();

        assert!(
            out.records.is_empty(),
            "a missing target yields no records: {:?}",
            out.records
        );
        assert!(
            out.section_failures.is_empty(),
            "a missing target is not a section failure: {:?}",
            out.section_failures
        );
    }

    /// F5: an ambiguous heading (duplicate identical headings in one doc) is
    /// warn-and-omitted at the MCP layer — no error, no section failure (a
    /// resolvable sibling heading still resolved), and the sibling is returned.
    #[test]
    fn section_ambiguous_heading_warns_omits_siblings_return() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-get-ambig-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(
            root.join("d.md"),
            "---\ntype: note\n---\n## Dup\nfirst\n## Dup\nsecond\n## Other\nkeep\n",
        )
        .unwrap();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let out = handle_output(
            &ctx,
            GetParams {
                targets: vec!["d".into()],
                section: vec!["Dup".into(), "Other".into()],
                ..Default::default()
            },
        )
        .expect("an ambiguous heading is warn-and-omit, not an error");
        let out = out.value();

        assert_eq!(out.records.len(), 1);
        let rec = &out.records[0];
        assert_eq!(
            rec["sections"]["Other"].as_str().unwrap(),
            "## Other\nkeep\n",
            "the sibling heading still resolves"
        );
        assert!(
            rec["sections"].get("Dup").is_none(),
            "the ambiguous heading is omitted: {}",
            rec["sections"]
        );
        assert!(
            out.section_failures.is_empty(),
            "a resolvable sibling means the target did not all-miss: {:?}",
            out.section_failures
        );
    }

    /// F3: `{limit, no_limit}` together is a params error (mirroring the CLI's
    /// clap conflict), refused before any work — not a silent truncation.
    #[test]
    fn limit_and_no_limit_together_is_params_error() {
        let (_tmp, root) = sectioned_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let result = handle(
            &ctx,
            GetParams {
                targets: vec!["a".into(), "b".into(), "c".into()],
                limit: Some(5),
                no_limit: true,
                ..Default::default()
            },
        );
        assert!(
            result.is_err(),
            "limit + no_limit must be a params error, got {result:?}"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("--limit") && err.contains("--no-limit"),
            "error must name the mutually-exclusive flags, got: {err}"
        );
    }

    /// F3: `no_limit` alone (its documented default-equivalent behavior) is
    /// accepted and returns every named target — no truncation, no error.
    #[test]
    fn no_limit_alone_returns_all_targets() {
        let (_tmp, root) = sectioned_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            GetParams {
                targets: vec!["a".into(), "b".into(), "c".into()],
                no_limit: true,
                ..Default::default()
            },
        )
        .expect("no_limit alone is accepted");
        assert_eq!(report.records.len(), 3, "every named target is returned");
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

    /// NRN-214: a get whose target does not resolve maps to `isError: true` and
    /// surfaces the `error:` diagnostic in `notes` — the MCP twin of the CLI's
    /// exit-1. The (empty) records still ship, so a batch's good targets survive.
    #[test]
    fn missing_target_maps_to_iserror_with_notes() {
        let (_tmp, root) = sectioned_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let result = handle_output(
            &ctx,
            GetParams {
                targets: vec!["does-not-exist".into()],
                ..Default::default()
            },
        )
        .expect("a missing target returns Ok(MutationResult), not Err");

        assert!(
            result.is_error(),
            "an unresolved target maps to isError:true"
        );
        let out = result.value();
        assert!(out.records.is_empty(), "no records for a missing target");
        assert!(
            out.notes.iter().any(|n| n.starts_with("error:")),
            "notes carry the error: diagnostic: {:?}",
            out.notes
        );
    }

    /// NRN-214: a get that resolves every target is `isError: false` with no
    /// `error:` notes — the success path is unchanged.
    #[test]
    fn resolved_target_is_not_error() {
        let (_tmp, root) = sectioned_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let result = handle_output(
            &ctx,
            GetParams {
                targets: vec!["doc".into()],
                ..Default::default()
            },
        )
        .expect("a resolved get succeeds");

        assert!(!result.is_error(), "a resolved get is isError:false");
        let out = result.value();
        assert_eq!(out.records.len(), 1, "the resolved record ships");
        assert!(
            out.notes.iter().all(|n| !n.starts_with("error:")),
            "no error notes on a clean get: {:?}",
            out.notes
        );
    }
}
