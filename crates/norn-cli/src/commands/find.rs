//! `norn find` — the filtered/sorted/paged document query.
//!
//! The command module owns its clap `Args`, the `to_params` mapping into the
//! wire vocabulary, and `run`: help-gate → summon → `FindParams` → render the
//! `FindReport`. Grammar + help text are donor-exact (NRN-329); the rendering
//! (paths / records / json / jsonl) is byte-faithful to the donor (`src/find/`).
//!
//! Deep facets (`.headings`, `.outgoing_links`, `.unresolved_links`,
//! `.incoming_links`) and `--all-cols` load the matches' full connection sets
//! (NRN-347). The CLI sets [`FindParams::with_connections`] only when one of
//! those facets is requested, so an unrequested deep facet is never rendered as
//! a misleading empty array — the empty-vs-loaded distinction lives here, where
//! the request is known.

use std::io::Write;

use clap::Args;
use norn_wire::{FindDoc, FindParams, FindReport, SortPaginateParams};

use crate::cli::GlobalArgs;
use crate::commands::args::{FilterArgs, SortPaginateArgs};
use crate::display::{Presenter, EXIT_OK, EXIT_OPERATIONAL, EXIT_USAGE};
use crate::output::palette::{self, Palette};
use crate::output::primitives::{count_line, record_block, separator, Field};
use crate::output::projection::{
    filter_frontmatter, frontmatter_to_display, headings_to_display, incoming_links_to_display,
    json_value_inline, outgoing_links_to_display, split_cols, unknown_facet_message,
    unresolved_links_to_display, warn_col_ignored, KNOWN_FACETS,
};

/// The deep connection facets that require a per-match connection load. When any
/// is requested (or `--all-cols`), the CLI sets `with_connections` so the owner
/// loads them; otherwise a plain `find` never pays that cost.
const DEEP_FACETS: &[&str] = &[
    "headings",
    "outgoing_links",
    "unresolved_links",
    "incoming_links",
];

const NAME: &str = "find";

#[derive(Args, Debug)]
pub struct FindArgs {
    // ── Filter predicates ──────────────────────────────────────────────
    #[command(flatten)]
    pub filter: FilterArgs,

    /// Return every document — escape hatch when no predicate is specified.
    /// Without --all and without any predicate, `norn find` prints its help
    /// page (a full-vault dump is almost always a mistake; require opt-in).
    #[arg(long, help_heading = "Filter options")]
    pub all: bool,

    // ── Sort / limit / paging (shared with `get`) ───────────────────────
    #[command(flatten)]
    pub paging: SortPaginateArgs,

    // ── Output ───────────────────────────────────────────────────────────
    /// Output format. Default auto-detects: TTY → records, piped → paths.
    #[arg(long, value_enum, help_heading = "Output")]
    pub format: Option<FindFormat>,

    /// Emit the full structured dump for each match: whole frontmatter plus
    /// every cache-served facet (`.headings`, the three link sets, `.body`).
    /// Competes with `--col` over the projection; the last of the two given wins.
    #[arg(long = "all-cols", overrides_with = "col", help_heading = "Output")]
    pub all_cols: bool,

    /// Comma-separated columns to include. Bare names select frontmatter
    /// fields (e.g. `status,title`), exactly like `norn get`. Structural
    /// facets are dot-prefixed: `.path`, `.stem`, `.frontmatter` (the whole
    /// block), `.headings`, `.outgoing_links`, `.unresolved_links`,
    /// `.incoming_links`, `.body`, `.document_hash` (the content hash
    /// `edit --expected-hash` wants; opt-in only — never in `--all-cols`).
    /// Default (no --col): frontmatter
    /// only. Ignored with a warning on paths format.
    #[arg(
        long,
        value_name = "COL1,COL2,...",
        value_delimiter = ',',
        overrides_with = "all_cols",
        help_heading = "Output"
    )]
    pub col: Vec<String>,

    /// Skip the pager even when stdout is a TTY.
    #[arg(long = "no-pager", help_heading = "Output")]
    pub no_pager: bool,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindFormat {
    Paths,
    Records,
    Json,
    Jsonl,
}

impl FindArgs {
    /// Parse the flags into the shared read-verb wire vocabulary.
    pub fn to_params(&self) -> (norn_wire::FilterParams, SortPaginateParams) {
        (self.filter.to_params(), self.paging.to_params())
    }

    /// Whether the request needs each match's deep connection facets loaded —
    /// true for `--all-cols` or any deep `--col` facet.
    fn wants_connections(&self) -> bool {
        if self.all_cols {
            return true;
        }
        let (facets, _fields) = split_cols(&self.col);
        facets.iter().any(|f| DEEP_FACETS.contains(&f.as_str()))
    }

    /// Whether any filter predicate is present (an empty `--text` is not one).
    /// Compared against the empty default so a new predicate flag can never be
    /// silently missed (donor `has_predicate`).
    fn has_predicate(&self) -> bool {
        let mut probe = self.filter.clone();
        if probe.text.as_deref() == Some("") {
            probe.text = None;
        }
        probe != FilterArgs::default()
    }
}

/// Present the command's outcome and return the process exit code.
pub fn run<O: Write, E: Write>(
    args: &FindArgs,
    global: &GlobalArgs,
    presenter: &mut Presenter<O, E>,
) -> i32 {
    // Help gate: bare `find` (no predicate, no --all) prints its help and exits
    // 2 — a full-vault dump is almost always a mistake (donor parity).
    if !args.all && !args.has_predicate() {
        let help = crate::help::render_command_long(NAME, global.color);
        let _ = presenter.streams().1.write_all(&help);
        return EXIT_USAGE;
    }

    let mut session = match crate::routed::open_session(global) {
        Ok(s) => s,
        Err(msg) => {
            presenter.diagnostic(&msg);
            return EXIT_OPERATIONAL;
        }
    };

    let (filter, paging) = args.to_params();
    let params = FindParams {
        filter,
        paging,
        with_connections: args.wants_connections(),
    };
    let report = match session.find(params) {
        Ok(r) => r,
        Err(e) => {
            presenter.diagnostic(&e.to_string());
            return EXIT_OPERATIONAL;
        }
    };

    let palette = palette::resolve(global.color);
    let (out, err) = presenter.streams();
    if let Err(e) = render(args, &report, &palette, out, err) {
        let _ = writeln!(err, "norn: {e}");
        return EXIT_OPERATIONAL;
    }
    EXIT_OK
}

/// Resolve the effective output format: explicit `--format` wins; otherwise TTY
/// → records, piped → paths (donor `resolve_format`).
fn resolve_format(explicit: Option<FindFormat>) -> FindFormat {
    match explicit {
        Some(f) => f,
        None => {
            if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                FindFormat::Records
            } else {
                FindFormat::Paths
            }
        }
    }
}

fn render(
    args: &FindArgs,
    report: &FindReport,
    palette: &Palette,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> std::io::Result<()> {
    let format = resolve_format(args.format);
    match format {
        FindFormat::Paths => render_paths(report, out, err)?,
        FindFormat::Json => render_json(args, report, out)?,
        FindFormat::Jsonl => render_jsonl(args, report, out, err)?,
        FindFormat::Records => render_records(args, report, palette, out)?,
    }
    warn_col_ignored(
        &args.col,
        (format == FindFormat::Paths).then_some("paths"),
        err,
    )?;
    warn_unknown_cols(report, &args.col, err)?;
    Ok(())
}

fn render_paths(
    report: &FindReport,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> std::io::Result<()> {
    for doc in &report.documents {
        writeln!(out, "{}", doc.path)?;
    }
    if report.truncated {
        writeln!(
            err,
            "note: showing {} of {} (--no-limit for all)",
            report.returned, report.total
        )?;
    }
    Ok(())
}

fn render_json(args: &FindArgs, report: &FindReport, out: &mut dyn Write) -> std::io::Result<()> {
    let documents: Vec<serde_json::Value> = report
        .documents
        .iter()
        .map(|d| doc_to_json(d, &args.col, args.all_cols))
        .collect();
    let payload = serde_json::json!({
        "total": report.total,
        "returned": report.returned,
        "starts_at": report.starts_at,
        "documents": documents,
    });
    writeln!(out, "{}", serde_json::to_string_pretty(&payload)?)
}

fn render_jsonl(
    args: &FindArgs,
    report: &FindReport,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> std::io::Result<()> {
    for doc in &report.documents {
        let line = doc_to_json(doc, &args.col, args.all_cols);
        writeln!(out, "{}", serde_json::to_string(&line)?)?;
    }
    if report.truncated {
        writeln!(
            err,
            "note: showing {} of {} (--no-limit for all)",
            report.returned, report.total
        )?;
    }
    Ok(())
}

fn render_records(
    args: &FindArgs,
    report: &FindReport,
    palette: &Palette,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let term_width = terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80);

    count_line(
        out,
        palette,
        report.total,
        report.returned,
        report.starts_at,
        "documents",
    )?;

    if !report.documents.is_empty() {
        writeln!(out)?;
    }

    let sort_field = args.paging.sort.as_deref();

    for (i, doc) in report.documents.iter().enumerate() {
        if i > 0 {
            separator(out, palette, term_width)?;
        }
        let pairs = build_record_pairs(doc, &args.col, args.all_cols);
        let fields: Vec<Field<'_>> = pairs
            .iter()
            .map(|(k, v)| Field {
                label: k.as_str(),
                value: v.as_str(),
                highlight: sort_field.is_some_and(|sf| sf == k),
            })
            .collect();
        record_block(out, palette, Some(doc.path.as_str()), &fields, term_width)?;
        if pairs.is_empty() {
            let placeholder = if args.col.is_empty() {
                "(no frontmatter)"
            } else {
                "(no matching fields)"
            };
            writeln!(
                out,
                "  {}{placeholder}{}",
                palette.dim.render(),
                palette.dim.render_reset()
            )?;
        }
    }
    Ok(())
}

/// The per-document JSON object under `--col` (donor `doc_to_json`). Deep facets
/// render empty until the deep-fetch port.
fn doc_to_json(doc: &FindDoc, cols: &[String], all_cols: bool) -> serde_json::Value {
    if cols.is_empty() && !all_cols {
        return serde_json::json!({
            "path": doc.path,
            "frontmatter": filter_frontmatter(doc.frontmatter.as_ref(), &[]),
        });
    }

    let (facets, fields) = split_cols(cols);
    let allow: std::collections::HashSet<&str> = facets.iter().map(String::as_str).collect();
    let mut obj = serde_json::json!({ "path": doc.path });
    let map = obj.as_object_mut().unwrap();

    if allow.contains("stem") {
        map.insert("stem".into(), serde_json::Value::String(doc.stem.clone()));
    }
    if allow.contains("document_hash") && !doc.hash.is_empty() {
        map.insert(
            "document_hash".into(),
            serde_json::Value::String(doc.hash.clone()),
        );
    }
    if all_cols || allow.contains("frontmatter") {
        map.insert(
            "frontmatter".into(),
            filter_frontmatter(doc.frontmatter.as_ref(), &[]),
        );
    } else if !fields.is_empty() {
        map.insert(
            "frontmatter".into(),
            filter_frontmatter(doc.frontmatter.as_ref(), &fields),
        );
    }
    // Deep facets: emit the pre-serialized values verbatim (byte-identical to
    // the cache's own `Heading`/`Link`/`IncomingLink` serialization). Populated
    // only when connections were loaded (`--all-cols` or a deep `--col` facet).
    for (facet, values) in [
        ("headings", &doc.headings),
        ("outgoing_links", &doc.outgoing_links),
        ("unresolved_links", &doc.unresolved_links),
        ("incoming_links", &doc.incoming_links),
    ] {
        if all_cols || allow.contains(facet) {
            map.insert(facet.into(), serde_json::Value::Array(values.clone()));
        }
    }
    if all_cols || allow.contains("body") {
        map.insert(
            "body".into(),
            serde_json::Value::String(doc.body_text.clone()),
        );
    }
    obj
}

/// The ordered `(label, value)` record rows for one doc (donor
/// `build_record_pairs`). Deep facets contribute nothing until deep-fetch lands.
fn build_record_pairs(doc: &FindDoc, cols: &[String], all_cols: bool) -> Vec<(String, String)> {
    let fm_object = doc.frontmatter.as_ref().and_then(|fm| fm.as_object());

    if cols.is_empty() && !all_cols {
        let mut pairs = Vec::new();
        if let Some(obj) = fm_object {
            for (key, value) in obj {
                pairs.push((key.clone(), json_value_inline(value)));
            }
        }
        return pairs;
    }

    let (facets, fields) = split_cols(cols);
    let facet_set: std::collections::HashSet<&str> = facets.iter().map(String::as_str).collect();
    let mut pairs = Vec::new();

    if facet_set.contains("stem") {
        pairs.push(("stem".into(), doc.stem.clone()));
    }
    if facet_set.contains("document_hash") && !doc.hash.is_empty() {
        pairs.push(("document_hash".into(), doc.hash.clone()));
    }
    if all_cols {
        if let Some(obj) = fm_object {
            for (key, value) in obj {
                pairs.push((key.clone(), json_value_inline(value)));
            }
        }
    }
    for field in &fields {
        if let Some(value) = fm_object.and_then(|obj| obj.get(field)) {
            pairs.push((field.clone(), json_value_inline(value)));
        }
    }
    if facet_set.contains("frontmatter") {
        if let Some(fm) = &doc.frontmatter {
            let value = frontmatter_to_display(fm);
            if !value.is_empty() {
                pairs.push(("frontmatter".into(), value));
            }
        }
    }
    // Deep facets, in the donor's field order; each emitted only when requested
    // (or `--all-cols`) AND non-empty (an empty facet contributes no row).
    if (all_cols || facet_set.contains("headings")) && !doc.headings.is_empty() {
        pairs.push(("headings".into(), headings_to_display(&doc.headings)));
    }
    if (all_cols || facet_set.contains("outgoing_links")) && !doc.outgoing_links.is_empty() {
        pairs.push((
            "outgoing_links".into(),
            outgoing_links_to_display(&doc.outgoing_links),
        ));
    }
    if (all_cols || facet_set.contains("unresolved_links")) && !doc.unresolved_links.is_empty() {
        pairs.push((
            "unresolved_links".into(),
            unresolved_links_to_display(&doc.unresolved_links),
        ));
    }
    if (all_cols || facet_set.contains("incoming_links")) && !doc.incoming_links.is_empty() {
        pairs.push((
            "incoming_links".into(),
            incoming_links_to_display(&doc.incoming_links),
        ));
    }
    if all_cols || facet_set.contains("body") {
        let body = doc.body_text.trim();
        if !body.is_empty() {
            pairs.push(("body".into(), body.to_string()));
        }
    }
    pairs
}

/// Warn for unresolved `--col` tokens: an unknown dot-facet, or a bare field
/// absent from every match (donor `warn_unknown_cols`).
fn warn_unknown_cols(
    report: &FindReport,
    cols: &[String],
    err: &mut dyn Write,
) -> std::io::Result<()> {
    let (facets, fields) = split_cols(cols);
    for facet in &facets {
        if !KNOWN_FACETS.contains(&facet.as_str()) {
            writeln!(err, "warning: {}", unknown_facet_message(facet))?;
        }
    }
    for field in &fields {
        let present_in_any = report.documents.iter().any(|d| {
            d.frontmatter
                .as_ref()
                .and_then(|fm| fm.as_object())
                .is_some_and(|obj| obj.contains_key(field))
        });
        if !present_in_any {
            writeln!(
                err,
                "warning: --col field `{field}` not present in any matching document"
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

    fn find_args(argv: &[&str]) -> FindArgs {
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Find(a) => a,
            other => panic!("expected find, got {other:?}"),
        }
    }

    #[test]
    fn find_format_parses() {
        let args = find_args(&["norn", "find", "--all", "--format", "json"]);
        assert_eq!(args.format, Some(FindFormat::Json));
    }

    #[test]
    fn all_cols_and_col_are_last_wins() {
        // NRN-331: --all-cols and --col compete over the projection; the last
        // one on the command line wins (no hard conflict).
        let col_last = find_args(&["norn", "find", "--all", "--all-cols", "--col", "title"]);
        assert!(!col_last.all_cols, "--col is last, so --all-cols is reset");
        assert_eq!(col_last.col, vec!["title".to_string()]);

        let all_cols_last = find_args(&["norn", "find", "--all", "--col", "title", "--all-cols"]);
        assert!(all_cols_last.all_cols, "--all-cols is last, so it wins");
        assert!(
            all_cols_last.col.is_empty(),
            "the overridden --col is reset"
        );
    }

    #[test]
    fn col_splits_on_comma() {
        let args = find_args(&["norn", "find", "--all", "--col", "title,status"]);
        assert_eq!(args.col, vec!["title".to_string(), "status".to_string()]);
    }

    #[test]
    fn has_predicate_false_for_bare_find() {
        let args = find_args(&["norn", "find", "--all"]);
        assert!(!args.has_predicate());
    }

    #[test]
    fn has_predicate_true_for_eq() {
        let args = find_args(&["norn", "find", "--eq", "type:note"]);
        assert!(args.has_predicate());
    }

    #[test]
    fn empty_text_is_not_a_predicate() {
        let args = find_args(&["norn", "find", "--text", ""]);
        assert!(!args.has_predicate());
    }

    fn doc(path: &str, fm: serde_json::Value) -> FindDoc {
        FindDoc {
            path: path.into(),
            stem: path.trim_end_matches(".md").into(),
            hash: "h".into(),
            frontmatter: if fm.is_null() { None } else { Some(fm) },
            body_text: "body".into(),
            headings: vec![],
            outgoing_links: vec![],
            unresolved_links: vec![],
            incoming_links: vec![],
        }
    }

    #[test]
    fn json_no_col_is_path_and_frontmatter_sorted() {
        let d = doc("a.md", serde_json::json!({"title": "A", "type": "note"}));
        let v = doc_to_json(&d, &[], false);
        // serde_json Value is a sorted map: keys frontmatter, path.
        let s = serde_json::to_string(&v).unwrap();
        assert_eq!(
            s,
            r#"{"frontmatter":{"title":"A","type":"note"},"path":"a.md"}"#
        );
    }

    #[test]
    fn json_col_bare_field_narrows_frontmatter() {
        let d = doc("a.md", serde_json::json!({"title": "A", "type": "note"}));
        let v = doc_to_json(&d, &["title".to_string()], false);
        let s = serde_json::to_string(&v).unwrap();
        assert_eq!(s, r#"{"frontmatter":{"title":"A"},"path":"a.md"}"#);
    }

    #[test]
    fn json_absent_frontmatter_is_null() {
        let d = doc("a.md", serde_json::Value::Null);
        let v = doc_to_json(&d, &[], false);
        let s = serde_json::to_string(&v).unwrap();
        assert_eq!(s, r#"{"frontmatter":null,"path":"a.md"}"#);
    }

    // ── NRN-347 deep facets: --all-cols / a deep --col loads connections ──

    #[test]
    fn wants_connections_only_for_deep_cols_or_all_cols() {
        assert!(find_args(&["norn", "find", "--all", "--all-cols"]).wants_connections());
        assert!(find_args(&["norn", "find", "--all", "--col", ".headings"]).wants_connections());
        assert!(
            find_args(&["norn", "find", "--all", "--col", ".incoming_links"]).wants_connections()
        );
        // Flat facets and bare fields never trigger the connection load.
        assert!(!find_args(&["norn", "find", "--all", "--col", ".stem"]).wants_connections());
        assert!(!find_args(&["norn", "find", "--all", "--col", "title"]).wants_connections());
        assert!(!find_args(&["norn", "find", "--all"]).wants_connections());
    }

    #[test]
    fn json_deep_facet_emits_serialized_values_verbatim() {
        let mut d = doc("a.md", serde_json::json!({"type": "note"}));
        d.headings = vec![serde_json::json!({"level": 2, "text": "Sec", "slug": "sec"})];
        let v = doc_to_json(&d, &[".headings".to_string()], false);
        // The heading value is emitted byte-for-byte under the facet key.
        assert_eq!(
            v["headings"],
            serde_json::json!([{"level": 2, "text": "Sec", "slug": "sec"}])
        );
    }

    #[test]
    fn records_deep_facet_folds_to_display_string() {
        let mut d = doc("a.md", serde_json::json!({"type": "note"}));
        d.headings = vec![serde_json::json!({"level": 2, "text": "Sec", "slug": "sec"})];
        let pairs = build_record_pairs(&d, &[".headings".to_string()], false);
        assert!(
            pairs.iter().any(|(k, v)| k == "headings" && v == "## Sec"),
            "expected a headings row rendered as '## Sec', got {pairs:?}"
        );
    }
}
