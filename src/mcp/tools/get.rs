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

/// Parameters for `vault.get`.
///
/// Mirrors the daily-useful slice of `norn get`'s flags: one or more targets and
/// an optional column request. The heavier CLI knobs (`--all-cols`, paging, the
/// byte-faithful `markdown` format) are intentionally omitted from v1 — the MCP
/// client always gets the full structured record set (frontmatter, headings, all
/// three link sets), and `col` only opts the on-request facets — `.body`, `.raw`,
/// and `.document_hash` — *in*.
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
    /// facet narrowing is not applied to the MCP envelope.
    #[serde(default)]
    pub col: Option<String>,
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
        // `get`'s defaults: no sort, no limit (return every named target),
        // 1-indexed paging start. These mirror `SortPaginateArgs`'s clap
        // defaults so MCP and CLI page identically.
        paging: SortPaginateArgs {
            sort: None,
            desc: false,
            limit: None,
            no_limit: false,
            starts_at: 1,
        },
        // Full structured dump is opt-in via `col`, not `all_cols`; v1 keeps the
        // cache-only `--all-cols` super-dump off the MCP surface.
        all_cols: false,
        col,
        // `--section` is not on the v1 MCP surface yet (see `GetParams` doc
        // comment) — no requested sections, so `show::run` never loads the
        // body for this reason alone.
        section: Vec::new(),
        // Records is `norn get`'s default format. The MCP wrapper serializes the
        // returned `ShowReport` to JSON regardless, so this only governs which
        // facets `show::run` loads (e.g. body), not a textual rendering.
        format: GetFormat::Records,
    };

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
}
