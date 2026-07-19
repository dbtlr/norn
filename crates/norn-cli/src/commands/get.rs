//! `norn get` — one or more documents in detail, over the shared sort/paging
//! surface. Same command-module pattern as `find`.
//!
//! Grammar + help text are donor-exact (NRN-329). The owner resolves the targets
//! and returns each document's full facet set (frontmatter, headings, the three
//! link sets, hash/stem, and body when asked); this module projects `--col` /
//! `--all-cols` / `--format` byte-faithfully to the donor (`src/get/render.rs`),
//! and forwards the resolution notes to stderr with an exit-1 on any `error:`
//! note. `--format markdown` prints the exact source bytes the owner read from
//! disk (ADR 0014: markdown is not part of the relational snapshot).

use std::collections::HashSet;
use std::io::Write;

use clap::Args;
use norn_wire::{GetParams, GetRecord, GetReport, SortPaginateParams};
use serde_json::{Map, Value};

use crate::cli::GlobalArgs;
use crate::commands::args::SortPaginateArgs;
use crate::display::{Presenter, EXIT_OK, EXIT_OPERATIONAL};
use crate::output::palette::Palette;
use crate::output::primitives::{record_block, separator, Field};
use crate::output::projection::{
    filter_frontmatter, frontmatter_to_display, headings_to_display, incoming_links_to_display,
    json_value_inline, outgoing_links_to_display, sections_to_json_object, split_cols,
    unknown_facet_message, unresolved_links_to_display, warn_col_ignored, warn_section_ignored,
    KNOWN_FACETS,
};

#[derive(Args, Debug)]
pub struct GetArgs {
    /// One or more doc targets. Each accepts path, stem, or wikilink-shaped
    /// input (with or without [[]]). Anchor / block-ref / pipe-alias
    /// suffixes are stripped before resolution.
    #[arg(required = true, num_args = 1.., value_name = "DOC")]
    pub targets: Vec<String>,

    // ── Sort / limit / paging (shared with `find`) ─────────────────────
    #[command(flatten)]
    pub paging: SortPaginateArgs,

    // ── Output ───────────────────────────────────────────────────────────
    /// Emit the full structured dump: every frontmatter field plus every
    /// cache-served facet (`.headings`, the three link sets, `.body`).
    /// Mutually exclusive with `--col`.
    #[arg(long = "all-cols", conflicts_with = "col", help_heading = "Output")]
    pub all_cols: bool,

    /// Comma-separated columns to include. Bare names select frontmatter
    /// fields (e.g. `status,title`), exactly like `norn find`. Structural
    /// facets are dot-prefixed: `.path`, `.stem`, `.frontmatter` (the whole
    /// block), `.headings`, `.outgoing_links`, `.unresolved_links`,
    /// `.incoming_links`, `.body`, `.document_hash` (the content hash
    /// `edit --expected-hash` wants; opt-in only — never in `--all-cols`).
    /// Without --col, frontmatter +
    /// headings + links are emitted (body only with --all-cols or `--col .body`).
    #[arg(
        long,
        value_name = "COL1,COL2,...",
        value_delimiter = ',',
        help_heading = "Output"
    )]
    pub col: Vec<String>,

    /// Named section to read, by exact heading text. Repeatable — pass once
    /// per section (`--section "Task Description" --section "Annotations"`).
    /// Each occurrence is one whole heading string, so a heading that itself
    /// contains a comma (`--section "Risks, Open Questions"`) is addressable
    /// verbatim — the same way `edit` takes a heading as one whole string.
    /// Resolved with the same boundary and failure semantics as
    /// `edit --append-to-section` / `--replace-section` (heading line through
    /// the next same-or-higher heading, or EOF) — a section read mirrors a
    /// section write. Orthogonal to `--col`/`--all-cols`; combine freely. A
    /// heading missing or ambiguous in a given document warns on stderr and is
    /// omitted from that document's `sections` (siblings and other documents
    /// are unaffected); if none of the requested headings resolve for a
    /// document, that is a hard failure (nonzero exit) for that target,
    /// mirroring how `get` already treats a target that fails to resolve at
    /// all. Ignored (with a warning) by `--format paths`/`markdown`, like
    /// `--col`.
    #[arg(
        long,
        value_name = "HEADING",
        num_args = 1,
        action = clap::ArgAction::Append,
        help_heading = "Output"
    )]
    pub section: Vec<String>,

    /// Output format. Default records; markdown returns one exact source file.
    #[arg(long, value_enum, default_value_t = GetFormat::Records, help_heading = "Output")]
    pub format: GetFormat,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum GetFormat {
    /// Vertical key-value record block per document.
    Records,
    /// One document path per line (`--col` is ignored).
    Paths,
    /// A single JSON array of record objects.
    Json,
    /// One JSON record object per line.
    Jsonl,
    /// The single selected document, byte-faithful from disk. Errors unless
    /// exactly one document is selected; `--col` is ignored.
    Markdown,
}

impl GetArgs {
    /// Parse the sort/paging flags into the shared wire vocabulary.
    pub fn to_params(&self) -> SortPaginateParams {
        self.paging.to_params()
    }

    /// Whether the whole-body field should be displayed — `--all-cols` or a
    /// `--col .body` request. Drives [`GetParams::with_body`].
    fn wants_body_display(&self) -> bool {
        if self.all_cols {
            return true;
        }
        let (facets, _fields) = split_cols(&self.col);
        facets.iter().any(|f| f == "body")
    }

    /// Whether this format consumes `--section` (records / json / jsonl). `paths`
    /// / `markdown` document it as ignored, so the CLI does not send it — that
    /// keeps the owner from resolving a heading whose miss would push an
    /// exit-flipping `error:` note into a format that never renders sections.
    fn format_consumes_sections(&self) -> bool {
        matches!(
            self.format,
            GetFormat::Records | GetFormat::Json | GetFormat::Jsonl
        )
    }
}

/// Present the command's outcome and return the process exit code.
pub fn run<O: Write, E: Write>(
    args: &GetArgs,
    global: &GlobalArgs,
    presenter: &mut Presenter<O, E>,
) -> i32 {
    let mut session = match crate::routed::open_session(global) {
        Ok(s) => s,
        Err(msg) => {
            presenter.diagnostic(&msg);
            return EXIT_OPERATIONAL;
        }
    };

    let params = GetParams {
        targets: args.targets.clone(),
        paging: args.to_params(),
        sections: if args.format_consumes_sections() {
            args.section.clone()
        } else {
            Vec::new()
        },
        with_body: args.wants_body_display(),
        markdown: matches!(args.format, GetFormat::Markdown),
    };

    let report = match session.get(params) {
        Ok(r) => r,
        Err(e) => {
            presenter.diagnostic(&e.to_string());
            return EXIT_OPERATIONAL;
        }
    };

    let (out, err) = presenter.streams();
    if matches!(args.format, GetFormat::Markdown) {
        emit_markdown(args, &report, out, err)
    } else {
        emit_structured(args, &report, out, err)
    }
}

/// Whether the report carries an `error:`-prefixed note — the single get
/// "failure" signal driving exit 1 (donor `ShowReport::has_error`).
fn has_error(report: &GetReport) -> bool {
    report.notes.iter().any(|n| n.starts_with("error:"))
}

/// The structured formats (records / json / jsonl / paths): render stdout, then
/// warnings + notes on stderr, then exit 1 on any `error:` note.
fn emit_structured<O: Write, E: Write>(
    args: &GetArgs,
    report: &GetReport,
    out: &mut O,
    err: &mut E,
) -> i32 {
    let text = match args.format {
        GetFormat::Json => render_json(report, &args.col),
        GetFormat::Jsonl => render_jsonl(report, &args.col),
        GetFormat::Paths => render_paths(report),
        GetFormat::Records => render_records(report, &args.col),
        GetFormat::Markdown => unreachable!("markdown handled by emit_markdown"),
    };
    // Exactly one trailing newline (donor `emit`).
    if text.ends_with('\n') {
        let _ = write!(out, "{text}");
    } else {
        let _ = writeln!(out, "{text}");
    }

    let paths_inert = matches!(args.format, GetFormat::Paths).then_some("paths");
    let _ = warn_col_ignored(&args.col, paths_inert, err);
    let _ = warn_section_ignored(&args.section, paths_inert, err);
    let _ = warn_unknown_cols(&args.col, report, err);
    for note in &report.notes {
        let _ = writeln!(err, "{note}");
    }

    if has_error(report) {
        EXIT_OPERATIONAL
    } else {
        EXIT_OK
    }
}

/// `--format markdown`: the exact source bytes the owner read from disk. Errors
/// unless exactly one document is selected; `--col`/`--section` are ignored (with
/// a warning). Byte-faithful — no trailing-newline fixup.
fn emit_markdown<O: Write, E: Write>(
    args: &GetArgs,
    report: &GetReport,
    out: &mut O,
    err: &mut E,
) -> i32 {
    let _ = warn_col_ignored(&args.col, Some("markdown"), err);
    let _ = warn_section_ignored(&args.section, Some("markdown"), err);
    for note in &report.notes {
        let _ = writeln!(err, "{note}");
    }

    if let Some(content) = &report.markdown_content {
        let _ = write!(out, "{content}");
        return if has_error(report) {
            EXIT_OPERATIONAL
        } else {
            EXIT_OK
        };
    }

    match report.records.len() {
        // Zero resolved: the per-target `error:` notes are already printed.
        0 => EXIT_OPERATIONAL,
        // One resolved but no content: the owner's source-read failed and pushed
        // an `error:` note (already printed above).
        1 => EXIT_OPERATIONAL,
        n => {
            let _ = writeln!(
                err,
                "error: --format markdown returns a single document; {n} selected \
                 — request one target at a time"
            );
            EXIT_OPERATIONAL
        }
    }
}

// ── JSON / JSONL ─────────────────────────────────────────────────────────────

/// JSON output: a single compact array of record objects, `--col`-narrowed.
fn render_json(report: &GetReport, cols: &[String]) -> String {
    let array: Vec<Value> = report
        .records
        .iter()
        .map(|r| record_to_json(r, cols))
        .collect();
    serde_json::to_string(&array).unwrap_or_else(|_| "[]".to_string())
}

/// JSONL output: one compact record object per line — the streaming sibling of
/// [`render_json`].
fn render_jsonl(report: &GetReport, cols: &[String]) -> String {
    let mut buf = String::new();
    for record in &report.records {
        let line = record_to_json(record, cols);
        buf.push_str(&serde_json::to_string(&line).unwrap_or_default());
        buf.push('\n');
    }
    buf
}

/// Project one record to its JSON object. `path` is always present as identity
/// context. With no `--col`, the full default set is emitted (frontmatter +
/// headings + the three link sets, plus body only when the owner carried one);
/// with `--col`, only the requested facets/fields (donor `narrow_to_json`).
fn record_to_json(rec: &GetRecord, cols: &[String]) -> Value {
    let mut map = Map::new();
    map.insert("path".into(), Value::String(rec.path.clone()));

    if cols.is_empty() {
        map.insert(
            "frontmatter".into(),
            rec.frontmatter.clone().unwrap_or(Value::Null),
        );
        map.insert("headings".into(), Value::Array(rec.headings.clone()));
        map.insert(
            "outgoing_links".into(),
            Value::Array(rec.outgoing_links.clone()),
        );
        map.insert(
            "unresolved_links".into(),
            Value::Array(rec.unresolved_links.clone()),
        );
        map.insert(
            "incoming_links".into(),
            Value::Array(rec.incoming_links.clone()),
        );
        // Body appears only when the owner carried it (`--all-cols` / `.body`);
        // hash/stem never appear in the no-`--col` dump (opt-in facets only).
        if let Some(body) = &rec.body {
            map.insert("body".into(), Value::String(body.clone()));
        }
        insert_sections(&mut map, rec);
        return Value::Object(map);
    }

    let (facets, fields) = split_cols(cols);
    let allow: HashSet<&str> = facets.iter().map(String::as_str).collect();

    if allow.contains("stem") {
        map.insert("stem".into(), Value::String(rec.stem.clone()));
    }
    if allow.contains("document_hash") && !rec.hash.is_empty() {
        map.insert("document_hash".into(), Value::String(rec.hash.clone()));
    }
    if allow.contains("frontmatter") {
        map.insert(
            "frontmatter".into(),
            rec.frontmatter.clone().unwrap_or(Value::Null),
        );
    } else if !fields.is_empty() {
        map.insert(
            "frontmatter".into(),
            filter_frontmatter(rec.frontmatter.as_ref(), &fields),
        );
    }
    if allow.contains("headings") {
        map.insert("headings".into(), Value::Array(rec.headings.clone()));
    }
    if allow.contains("outgoing_links") {
        map.insert(
            "outgoing_links".into(),
            Value::Array(rec.outgoing_links.clone()),
        );
    }
    if allow.contains("unresolved_links") {
        map.insert(
            "unresolved_links".into(),
            Value::Array(rec.unresolved_links.clone()),
        );
    }
    if allow.contains("incoming_links") {
        map.insert(
            "incoming_links".into(),
            Value::Array(rec.incoming_links.clone()),
        );
    }
    if allow.contains("body") {
        map.insert(
            "body".into(),
            rec.body.clone().map(Value::String).unwrap_or(Value::Null),
        );
    }
    insert_sections(&mut map, rec);
    Value::Object(map)
}

/// `--section` is orthogonal to `--col`/`--all-cols`: inserted whenever the
/// record carries it (i.e. `--section` was passed and this format consumes it).
fn insert_sections(map: &mut Map<String, Value>, rec: &GetRecord) {
    if let Some(sections) = &rec.sections {
        map.insert("sections".into(), sections_to_json_object(sections));
    }
}

// ── paths ────────────────────────────────────────────────────────────────────

fn render_paths(report: &GetReport) -> String {
    let mut buf = String::new();
    for record in &report.records {
        buf.push_str(&record.path);
        buf.push('\n');
    }
    buf
}

// ── records ──────────────────────────────────────────────────────────────────

/// `records` output: one vertical key-value block per document, separated by a
/// horizontal rule. Field order: frontmatter fields → headings → outgoing →
/// unresolved → incoming → body → sections. Empty facets are omitted.
fn render_records(report: &GetReport, cols: &[String]) -> String {
    // The donor renders get records uncolored regardless of TTY (a likely
    // oversight preserved for byte-parity); color isn't exercised by the parity
    // harness, which always runs piped (color off) anyway.
    let palette = Palette::off();
    let term_width = terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80);

    let (facets, field_cols) = split_cols(cols);
    let facet_set: HashSet<&str> = facets.iter().map(String::as_str).collect();
    let all_cols = cols.is_empty();

    let mut buf: Vec<u8> = Vec::new();
    for (i, record) in report.records.iter().enumerate() {
        if i > 0 {
            let _ = separator(&mut buf, &palette, term_width);
        }
        let owned = build_text_fields(record, all_cols, &facet_set, &field_cols);
        let fields: Vec<Field<'_>> = owned
            .iter()
            .map(|(k, v)| Field {
                label: k.as_str(),
                value: v.as_str(),
                highlight: false,
            })
            .collect();
        let _ = record_block(&mut buf, &palette, Some(&record.path), &fields, term_width);
        if fields.is_empty() {
            let _ = writeln!(buf, "  (no fields)");
        }
    }
    String::from_utf8(buf).unwrap_or_default()
}

/// Build the ordered `(label, value)` rows for one record (donor
/// `build_text_fields`).
fn build_text_fields(
    rec: &GetRecord,
    all_cols: bool,
    facet_set: &HashSet<&str>,
    field_cols: &[String],
) -> Vec<(String, String)> {
    let mut fields: Vec<(String, String)> = Vec::new();
    let fm_object = rec.frontmatter.as_ref().and_then(Value::as_object);

    if facet_set.contains("stem") {
        fields.push(("stem".into(), rec.stem.clone()));
    }
    if facet_set.contains("document_hash") && !rec.hash.is_empty() {
        fields.push(("document_hash".into(), rec.hash.clone()));
    }
    // Bare --col names project individual frontmatter fields, in request order.
    if !field_cols.is_empty() {
        if let Some(obj) = fm_object {
            for key in field_cols {
                if let Some(value) = obj.get(key) {
                    fields.push((key.clone(), json_value_inline(value)));
                }
            }
        }
    }
    // Default (no --col): every frontmatter key as its own labeled line.
    if all_cols {
        if let Some(obj) = fm_object {
            for (key, value) in obj {
                fields.push((key.clone(), json_value_inline(value)));
            }
        }
    }
    // `.frontmatter` facet emits the whole consolidated block.
    if facet_set.contains("frontmatter") {
        if let Some(fm) = &rec.frontmatter {
            let value = frontmatter_to_display(fm);
            if !value.is_empty() {
                fields.push(("frontmatter".into(), value));
            }
        }
    }
    if (all_cols || facet_set.contains("headings")) && !rec.headings.is_empty() {
        fields.push(("headings".into(), headings_to_display(&rec.headings)));
    }
    if (all_cols || facet_set.contains("outgoing_links")) && !rec.outgoing_links.is_empty() {
        fields.push((
            "outgoing_links".into(),
            outgoing_links_to_display(&rec.outgoing_links),
        ));
    }
    if (all_cols || facet_set.contains("unresolved_links")) && !rec.unresolved_links.is_empty() {
        fields.push((
            "unresolved_links".into(),
            unresolved_links_to_display(&rec.unresolved_links),
        ));
    }
    if (all_cols || facet_set.contains("incoming_links")) && !rec.incoming_links.is_empty() {
        fields.push((
            "incoming_links".into(),
            incoming_links_to_display(&rec.incoming_links),
        ));
    }
    if all_cols || facet_set.contains("body") {
        if let Some(body) = &rec.body {
            if !body.trim().is_empty() {
                fields.push(("body".into(), body.trim().to_string()));
            }
        }
    }
    // `--section`: a distinct flag (not a `--col` facet) — rendered
    // unconditionally, one labeled block per requested heading in request order,
    // the verbatim span (byte-identical to `--format json`).
    if let Some(sections) = &rec.sections {
        for (heading, content) in sections {
            fields.push((heading.clone(), content.clone()));
        }
    }
    fields
}

/// Warn for `--col` tokens that won't resolve: an unknown dot-facet, or a bare
/// frontmatter field absent from every record (donor `warn_unknown_cols`). Note
/// the `warn:` prefix (get's donor uses `warn:`, not find's `warning:`).
fn warn_unknown_cols<E: Write>(
    cols: &[String],
    report: &GetReport,
    err: &mut E,
) -> std::io::Result<()> {
    let (facets, fields) = split_cols(cols);
    for facet in &facets {
        if !KNOWN_FACETS.contains(&facet.as_str()) {
            writeln!(err, "warn: {}", unknown_facet_message(facet))?;
        }
    }
    for field in &fields {
        let present_in_any = report.records.iter().any(|r| {
            r.frontmatter
                .as_ref()
                .and_then(Value::as_object)
                .is_some_and(|obj| obj.contains_key(field))
        });
        if !present_in_any {
            writeln!(
                err,
                "warn: --col field '{field}' not present in document (bare names select frontmatter fields; use '.{field}' for a structural facet)"
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use clap::Parser;
    use serde_json::json;

    fn get_args(argv: &[&str]) -> GetArgs {
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Get(a) => a,
            other => panic!("expected get, got {other:?}"),
        }
    }

    #[test]
    fn targets_are_collected_and_paging_defaults() {
        let args = get_args(&["norn", "get", "alpha", "notes/beta.md"]);
        assert_eq!(args.targets, vec!["alpha", "notes/beta.md"]);
        assert_eq!(args.to_params(), SortPaginateParams::default());
    }

    #[test]
    fn get_requires_at_least_one_target() {
        assert!(Cli::try_parse_from(["norn", "get"]).is_err());
    }

    #[test]
    fn all_cols_conflicts_with_col() {
        let res = Cli::try_parse_from(["norn", "get", "a.md", "--all-cols", "--col", "type"]);
        assert!(res.is_err(), "--all-cols and --col are mutually exclusive");
    }

    #[test]
    fn format_defaults_records() {
        let args = get_args(&["norn", "get", "a.md"]);
        assert_eq!(args.format, GetFormat::Records);
    }

    #[test]
    fn wants_body_display_only_for_all_cols_or_dot_body() {
        assert!(get_args(&["norn", "get", "a.md", "--all-cols"]).wants_body_display());
        assert!(get_args(&["norn", "get", "a.md", "--col", ".body"]).wants_body_display());
        assert!(!get_args(&["norn", "get", "a.md"]).wants_body_display());
        assert!(!get_args(&["norn", "get", "a.md", "--col", "title"]).wants_body_display());
    }

    fn record(path: &str, fm: Value) -> GetRecord {
        GetRecord {
            path: path.into(),
            stem: path.trim_end_matches(".md").into(),
            hash: "deadbeef".into(),
            frontmatter: if fm.is_null() { None } else { Some(fm) },
            headings: vec![json!({"level": 2, "text": "Sec", "slug": "sec"})],
            outgoing_links: vec![json!({"target": "b", "resolved_path": "b.md"})],
            unresolved_links: vec![],
            incoming_links: vec![],
            body: None,
            sections: None,
        }
    }

    #[test]
    fn json_default_emits_path_frontmatter_and_facets_no_body() {
        let rec = record("a.md", json!({"title": "A"}));
        let v = record_to_json(&rec, &[]);
        let obj = v.as_object().unwrap();
        assert!(obj.contains_key("path"));
        assert!(obj.contains_key("frontmatter"));
        assert!(obj.contains_key("headings"));
        assert!(obj.contains_key("outgoing_links"));
        assert!(obj.contains_key("incoming_links"));
        // No body/hash/stem in the default dump.
        assert!(!obj.contains_key("body"));
        assert!(!obj.contains_key("document_hash"));
        assert!(!obj.contains_key("stem"));
    }

    #[test]
    fn json_col_narrows_to_requested_facets_only() {
        let rec = record("a.md", json!({"title": "A"}));
        let v = record_to_json(&rec, &[".headings".to_string()]);
        let obj = v.as_object().unwrap();
        assert!(obj.contains_key("headings"));
        assert!(obj.contains_key("path"));
        assert!(!obj.contains_key("frontmatter"));
        assert!(!obj.contains_key("outgoing_links"));
    }

    #[test]
    fn json_col_document_hash_is_opt_in() {
        let rec = record("a.md", json!({"title": "A"}));
        let v = record_to_json(&rec, &[".document_hash".to_string()]);
        assert_eq!(v["document_hash"], json!("deadbeef"));
    }

    #[test]
    fn records_default_shows_frontmatter_then_facets() {
        let rec = record("a.md", json!({"title": "A", "type": "note"}));
        let report = GetReport {
            records: vec![rec],
            notes: vec![],
            markdown_content: None,
        };
        let text = render_records(&report, &[]);
        assert!(text.contains("a.md"), "path header: {text}");
        assert!(text.contains("title"), "frontmatter field: {text}");
        assert!(text.contains("headings"), "headings row: {text}");
    }

    #[test]
    fn has_error_drives_the_failure_signal() {
        let report = GetReport {
            records: vec![],
            notes: vec!["error: 'x' did not resolve to any doc".into()],
            markdown_content: None,
        };
        assert!(has_error(&report));
        let ok = GetReport {
            records: vec![],
            notes: vec!["note: 'x' resolved to 2 docs".into()],
            markdown_content: None,
        };
        assert!(!has_error(&ok));
    }
}
