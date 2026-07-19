//! The single render entry (NRN-370): one [`emit`] call turns a command's
//! returned [`Output`] into bytes, and it is the ONLY place the read/registry
//! verbs render. A verb resolves its report and returns an `Output`; `emit`
//! resolves the effective [`Format`] (isatty defaulting), resolves the palette
//! once, composes records through a [`Sink`], routes annotations through a
//! [`Conversation`], and derives the process exit code. A user error returns a
//! [`Diagnostic`] instead, rendered through the one presenter path.
//!
//! The renderers here are byte-faithful to the donor CLI: the `find` / `get`
//! projections run through the shared `output::projection` ladder; `count` /
//! `describe` / `vault list` reproduce their bespoke text unstyled (they never
//! resolved a palette in the donor, and are kept byte-identical).

use std::fmt::Write as _;
use std::io::{self, Write};

use norn_config::RegisteredVault;
use norn_wire::{
    CountReport, DescribeReport, FindDoc, FindReport, GetRecord, GetReport, GroupNode,
};
use serde::Serialize;
use serde_json::Value;

use crate::cli::GlobalArgs;
use crate::output::palette::{self, Palette};
use crate::output::primitives::Field;
use crate::output::projection::{
    project_json, project_pairs, split_cols, unknown_facet_message, warn_col_ignored,
    warn_section_ignored, DefaultCols, DocView, KNOWN_FACETS,
};

use super::conversation::Conversation;
use super::format::Format;
use super::output::{CountView, DescribeView, FindView, GetView, Output, VaultListView};
use super::sink::Sink;
use super::{Diagnostic, Presenter, EXIT_OK, EXIT_OPERATIONAL, EXIT_USAGE};

/// Whether the process stdout is a terminal — the one isatty read, consumed by
/// [`FormatSpec::resolve`](super::format::FormatSpec::resolve).
fn is_stdout_tty() -> bool {
    std::io::IsTerminal::is_terminal(&std::io::stdout())
}

/// The effective terminal width for record wrapping (donor default 80).
fn term_width() -> usize {
    terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80)
}

/// The one render IO-error policy (NRN-372), applied by every render path.
///
/// A render closure does its writes with `?` and, on success, returns the exit
/// code its content implies (e.g. `get`'s `has_error` outcome). This resolves
/// that result the same way for every verb:
/// - `BrokenPipe` (the reader end closed early — `norn find | head`) is
///   tolerated silently and treated as success. This is standard CLI
///   behavior; a downstream reader closing the pipe is not the vault's fault.
/// - Every other IO error (full disk, closed fd, …) is a real failure: one
///   `norn: <e>` diagnostic on stderr, and the operational exit.
///
/// No render path swallows an IO error with `let _ =` — every write funnels
/// through this one outcome.
fn render_outcome(result: io::Result<i32>, err: &mut dyn Write) -> i32 {
    match result {
        Ok(code) => code,
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => EXIT_OK,
        Err(e) => {
            let _ = writeln!(err, "norn: {e}");
            EXIT_OPERATIONAL
        }
    }
}

/// Render a command's returned [`Output`] (or its [`Diagnostic`]) and return the
/// process exit code. The single render seam: every read/registry verb reaches
/// stdout through here and nowhere else.
pub fn emit<O: Write, E: Write>(
    result: Result<Output, Diagnostic>,
    global: &GlobalArgs,
    presenter: &mut Presenter<O, E>,
) -> i32 {
    let output = match result {
        Ok(output) => output,
        Err(diag) => {
            presenter.present_diagnostic(&diag);
            return EXIT_OPERATIONAL;
        }
    };
    match output {
        Output::Find(view) => render_find(view, global, presenter),
        Output::Get(view) => render_get(view, global, presenter),
        Output::Count(view) => render_count(view, presenter),
        Output::Describe(view) => render_describe(view, presenter),
        Output::VaultList(view) => render_vault_list(view, presenter),
        Output::Line(line) => {
            let (out, err) = presenter.streams();
            let result: io::Result<i32> = (|| {
                writeln!(out, "{line}")?;
                Ok(EXIT_OK)
            })();
            render_outcome(result, err)
        }
        Output::Usage(bytes) => {
            let (_out, err) = presenter.streams();
            let result: io::Result<i32> = (|| {
                err.write_all(&bytes)?;
                Ok(EXIT_USAGE)
            })();
            render_outcome(result, err)
        }
    }
}

// ── find ───────────────────────────────────────────────────────────────────────

fn render_find<O: Write, E: Write>(
    view: FindView,
    global: &GlobalArgs,
    presenter: &mut Presenter<O, E>,
) -> i32 {
    let format = view.spec.resolve(view.explicit, is_stdout_tty());
    let palette = palette::resolve(global.color);
    let width = term_width();
    let (out, err) = presenter.streams();
    let mut conv = Conversation::new(err);

    let result: io::Result<i32> = (|| {
        match format {
            Format::Paths => {
                for doc in &view.report.documents {
                    writeln!(out, "{}", doc.path)?;
                }
                if view.report.truncated {
                    conv.line(&truncation_note(&view.report))?;
                }
            }
            Format::Json => render_find_json(out, &view)?,
            Format::Jsonl => {
                for doc in &view.report.documents {
                    let line = project_json(
                        &find_doc_view(doc),
                        &view.cols,
                        view.all_cols,
                        DefaultCols::FrontmatterOnly,
                    );
                    writeln!(out, "{}", serde_json::to_string(&line)?)?;
                }
                if view.report.truncated {
                    conv.line(&truncation_note(&view.report))?;
                }
            }
            Format::Records => {
                let mut sink = Sink::new(out, &palette, width);
                render_find_records(&mut sink, &view)?;
            }
            Format::Markdown => unreachable!("find has no markdown format"),
        }
        warn_col_ignored(
            &view.cols,
            (format == Format::Paths).then_some("paths"),
            conv.writer(),
        )?;
        warn_unknown_cols_find(&view.report, &view.cols, conv.writer())?;
        Ok(EXIT_OK)
    })();

    render_outcome(result, conv.writer())
}

/// The truncation note both `paths` and `jsonl` emit on stderr (donor parity).
fn truncation_note(report: &FindReport) -> String {
    format!(
        "note: showing {} of {} (--no-limit for all)",
        report.returned, report.total
    )
}

fn render_find_json(out: &mut dyn Write, view: &FindView) -> io::Result<()> {
    let documents: Vec<Value> = view
        .report
        .documents
        .iter()
        .map(|d| {
            project_json(
                &find_doc_view(d),
                &view.cols,
                view.all_cols,
                DefaultCols::FrontmatterOnly,
            )
        })
        .collect();
    let payload = serde_json::json!({
        "total": view.report.total,
        "returned": view.report.returned,
        "starts_at": view.report.starts_at,
        "documents": documents,
    });
    writeln!(out, "{}", serde_json::to_string_pretty(&payload)?)
}

fn render_find_records(sink: &mut Sink<'_>, view: &FindView) -> io::Result<()> {
    let report = &view.report;
    sink.count_line(report.total, report.returned, report.starts_at, "documents")?;
    if !report.documents.is_empty() {
        sink.blank_line()?;
    }
    let sort_field = view.sort_field.as_deref();
    for (i, doc) in report.documents.iter().enumerate() {
        if i > 0 {
            sink.separator()?;
        }
        let pairs = project_pairs(&find_doc_view(doc), &view.cols, view.all_cols);
        let fields: Vec<Field<'_>> = pairs
            .iter()
            .map(|(k, v)| Field {
                label: k.as_str(),
                value: v.as_str(),
                highlight: sort_field.is_some_and(|sf| sf == k),
            })
            .collect();
        sink.record_block(Some(doc.path.as_str()), &fields)?;
        if pairs.is_empty() {
            let placeholder = if view.cols.is_empty() {
                "(no frontmatter)"
            } else {
                "(no matching fields)"
            };
            let dim = sink.palette().dim;
            writeln!(
                sink.writer(),
                "  {}{placeholder}{}",
                dim.render(),
                dim.render_reset()
            )?;
        }
    }
    Ok(())
}

/// Warn for unresolved `--col` tokens: an unknown dot-facet, or a bare field
/// absent from every match (donor `find::warn_unknown_cols`, `warning:` prefix).
fn warn_unknown_cols_find(
    report: &FindReport,
    cols: &[String],
    err: &mut dyn Write,
) -> io::Result<()> {
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

fn find_doc_view(doc: &FindDoc) -> DocView<'_> {
    DocView {
        path: &doc.path,
        stem: &doc.stem,
        hash: &doc.hash,
        frontmatter: doc.frontmatter.as_ref(),
        headings: &doc.headings,
        outgoing_links: &doc.outgoing_links,
        unresolved_links: &doc.unresolved_links,
        incoming_links: &doc.incoming_links,
        // `find`'s wire record always carries a (possibly empty) body string.
        body: Some(&doc.body_text),
        sections: None,
    }
}

// ── get ────────────────────────────────────────────────────────────────────────

fn render_get<O: Write, E: Write>(
    view: GetView,
    global: &GlobalArgs,
    presenter: &mut Presenter<O, E>,
) -> i32 {
    let format = view.spec.resolve(view.explicit, is_stdout_tty());
    let (out, err) = presenter.streams();
    let mut conv = Conversation::new(err);

    if format == Format::Markdown {
        // Byte-faithful source passthrough — never colorized, so no palette.
        return render_get_markdown(&view, out, &mut conv);
    }

    let text = match format {
        Format::Json => render_get_json(&view.report, &view.cols),
        Format::Jsonl => render_get_jsonl(&view.report, &view.cols),
        Format::Paths => render_get_paths(&view.report),
        Format::Records => {
            // NRN-362: get records render through the SAME resolved palette find
            // uses. Piped (parity, pipelines) it resolves off → byte-unchanged.
            let palette = palette::resolve(global.color);
            render_get_records(&view.report, &palette, &view.cols)
        }
        Format::Markdown => unreachable!("markdown handled above"),
    };
    let result: io::Result<i32> = (|| {
        // Exactly one trailing newline (donor `emit`).
        if text.ends_with('\n') {
            write!(out, "{text}")?;
        } else {
            writeln!(out, "{text}")?;
        }

        let paths_inert = (format == Format::Paths).then_some("paths");
        warn_col_ignored(&view.cols, paths_inert, conv.writer())?;
        warn_section_ignored(&view.sections, paths_inert, conv.writer())?;
        warn_unknown_cols_get(&view.cols, &view.report, conv.writer())?;
        for note in &view.report.notes {
            conv.line(note)?;
        }

        Ok(if has_error(&view.report) {
            EXIT_OPERATIONAL
        } else {
            EXIT_OK
        })
    })();

    render_outcome(result, conv.writer())
}

/// `--format markdown`: the exact source bytes (donor `emit_markdown`). Errors
/// unless exactly one document resolved; `--col`/`--section` are ignored (warned).
fn render_get_markdown(view: &GetView, out: &mut dyn Write, conv: &mut Conversation<'_>) -> i32 {
    let result: io::Result<i32> = (|| {
        warn_col_ignored(&view.cols, Some("markdown"), conv.writer())?;
        warn_section_ignored(&view.sections, Some("markdown"), conv.writer())?;
        for note in &view.report.notes {
            conv.line(note)?;
        }

        if let Some(content) = &view.report.markdown_content {
            write!(out, "{content}")?;
            return Ok(if has_error(&view.report) {
                EXIT_OPERATIONAL
            } else {
                EXIT_OK
            });
        }

        Ok(match view.report.records.len() {
            // Zero resolved, or one resolved but no content (source-read
            // failure): the per-target `error:` notes are already printed.
            0 | 1 => EXIT_OPERATIONAL,
            n => {
                conv.line(&format!(
                    "error: --format markdown returns a single document; {n} selected \
                     — request one target at a time"
                ))?;
                EXIT_OPERATIONAL
            }
        })
    })();

    render_outcome(result, conv.writer())
}

/// The single get "failure" signal: an `error:`-prefixed note drives exit 1.
fn has_error(report: &GetReport) -> bool {
    report.notes.iter().any(|n| n.starts_with("error:"))
}

fn render_get_json(report: &GetReport, cols: &[String]) -> String {
    let array: Vec<Value> = report
        .records
        .iter()
        .map(|r| project_json(&get_record_view(r), cols, false, DefaultCols::FullFacets))
        .collect();
    serde_json::to_string(&array).unwrap_or_else(|_| "[]".to_string())
}

fn render_get_jsonl(report: &GetReport, cols: &[String]) -> String {
    let mut buf = String::new();
    for record in &report.records {
        let line = project_json(
            &get_record_view(record),
            cols,
            false,
            DefaultCols::FullFacets,
        );
        buf.push_str(&serde_json::to_string(&line).unwrap_or_default());
        buf.push('\n');
    }
    buf
}

fn render_get_paths(report: &GetReport) -> String {
    let mut buf = String::new();
    for record in &report.records {
        buf.push_str(&record.path);
        buf.push('\n');
    }
    buf
}

fn render_get_records(report: &GetReport, palette: &Palette, cols: &[String]) -> String {
    let width = term_width();
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut sink = Sink::new(&mut buf, palette, width);
        for (i, record) in report.records.iter().enumerate() {
            if i > 0 {
                let _ = sink.separator();
            }
            let pairs = project_pairs(&get_record_view(record), cols, cols.is_empty());
            let fields: Vec<Field<'_>> = pairs
                .iter()
                .map(|(k, v)| Field {
                    label: k.as_str(),
                    value: v.as_str(),
                    highlight: false,
                })
                .collect();
            let _ = sink.record_block(Some(&record.path), &fields);
            if fields.is_empty() {
                let _ = writeln!(sink.writer(), "  (no fields)");
            }
        }
    }
    String::from_utf8(buf).unwrap_or_default()
}

/// Warn for `--col` tokens that won't resolve (donor `get::warn_unknown_cols`,
/// `warn:` prefix — distinct from find's `warning:`).
fn warn_unknown_cols_get(
    cols: &[String],
    report: &GetReport,
    err: &mut dyn Write,
) -> io::Result<()> {
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

fn get_record_view(rec: &GetRecord) -> DocView<'_> {
    DocView {
        path: &rec.path,
        stem: &rec.stem,
        hash: &rec.hash,
        frontmatter: rec.frontmatter.as_ref(),
        headings: &rec.headings,
        outgoing_links: &rec.outgoing_links,
        unresolved_links: &rec.unresolved_links,
        incoming_links: &rec.incoming_links,
        body: rec.body.as_deref(),
        sections: rec.sections.as_deref(),
    }
}

// ── count ──────────────────────────────────────────────────────────────────────

fn render_count<O: Write, E: Write>(view: CountView, presenter: &mut Presenter<O, E>) -> i32 {
    let format = view.spec.resolve(view.explicit, is_stdout_tty());
    let text = match format {
        Format::Json => count_json(&view.report),
        _ => count_text(&view.report),
    };
    let (out, err) = presenter.streams();
    let result: io::Result<i32> = (|| {
        if text.ends_with('\n') {
            write!(out, "{text}")?;
        } else {
            writeln!(out, "{text}")?;
        }
        Ok(EXIT_OK)
    })();
    render_outcome(result, err)
}

fn count_json(report: &CountReport) -> String {
    serde_json::to_string(report).unwrap_or_else(|_| "{}".to_string())
}

fn count_text(report: &CountReport) -> String {
    let mut s = String::new();
    match report {
        CountReport::Total { total } => {
            let _ = writeln!(s, "total      {total}");
        }
        CountReport::Grouped { by, total, groups } => {
            let _ = writeln!(s, "total      {total}");
            let _ = writeln!(s);
            let header_width = by
                .len()
                .max(groups.keys().map(String::len).max().unwrap_or(0));
            let _ = writeln!(s, "{by:<header_width$}  count");
            for (key, count) in groups {
                let _ = writeln!(s, "{key:<header_width$}  {count}");
            }
        }
        CountReport::GroupedMulti { by, total, groups } => {
            let _ = writeln!(s, "total      {total}");
            let _ = writeln!(s);
            let _ = writeln!(s, "{}", by.join(" / "));
            count_group_tree(&mut s, groups, 0);
        }
    }
    s
}

fn count_group_tree(
    s: &mut String,
    groups: &std::collections::BTreeMap<String, GroupNode>,
    depth: usize,
) {
    let indent = "  ".repeat(depth);
    let leaf_width = groups
        .iter()
        .filter(|(_, node)| matches!(node, GroupNode::Leaf(_)))
        .map(|(key, _)| key.len())
        .max()
        .unwrap_or(0);
    for (key, node) in groups {
        match node {
            GroupNode::Leaf(count) => {
                let _ = writeln!(s, "{indent}{key:<leaf_width$}  {count}");
            }
            GroupNode::Branch(children) => {
                let _ = writeln!(s, "{indent}{key}");
                count_group_tree(s, children, depth + 1);
            }
        }
    }
}

// ── describe ─────────────────────────────────────────────────────────────────

fn render_describe<O: Write, E: Write>(view: DescribeView, presenter: &mut Presenter<O, E>) -> i32 {
    let format = view.spec.resolve(view.explicit, is_stdout_tty());
    let text = match format {
        Format::Json => describe_json(&view.report),
        _ => describe_text(&view.report),
    };
    let (out, err) = presenter.streams();
    let result: io::Result<i32> = (|| {
        if text.ends_with('\n') {
            write!(out, "{text}")?;
        } else {
            writeln!(out, "{text}")?;
        }
        Ok(EXIT_OK)
    })();
    render_outcome(result, err)
}

fn describe_json(report: &DescribeReport) -> String {
    serde_json::to_string(report).unwrap_or_else(|_| "{}".to_string())
}

fn describe_text(report: &DescribeReport) -> String {
    let mut s = String::new();
    if !report.folders.is_empty() {
        let _ = writeln!(s, "folders    {}", report.folders.len());
    }
    if !report.path_rules.is_empty() {
        let _ = writeln!(s, "path rules {}", report.path_rules.len());
    }
    if !report.creatable_rules.is_empty() {
        let _ = writeln!(s, "creatable  {}", report.creatable_rules.len());
    }
    if let Some(inbox) = &report.inbox {
        let _ = writeln!(s, "inbox      {inbox}");
    }
    if let Some(data) = &report.data {
        if !s.is_empty() {
            let _ = writeln!(s);
        }
        let dates = data
            .dates
            .iter()
            .map(|d| format!("{} {} → {}", d.field, d.min, d.max))
            .collect::<Vec<_>>()
            .join(" · ");
        if dates.is_empty() {
            let _ = writeln!(s, "{} documents", data.total);
        } else {
            let _ = writeln!(s, "{} documents · {}", data.total, dates);
        }
        let label_width = data.fields.iter().map(|f| f.field.len()).max().unwrap_or(0) + 1;
        for f in &data.fields {
            let body = f
                .values
                .iter()
                .map(|vc| format!("{} {}", vc.value, vc.count))
                .collect::<Vec<_>>()
                .join(" · ");
            let more = if f.more > 0 {
                format!(" (+{} more)", f.more)
            } else {
                String::new()
            };
            let _ = writeln!(s, "{:<label_width$} {body}{more}", format!("{}:", f.field));
        }
        if !data.skipped.is_empty() {
            let sk = data
                .skipped
                .iter()
                .map(|sf| format!("{} {}/{}", sf.field, sf.distinct, sf.total))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(s, "(skipped: {sk})");
        }
    }
    s
}

// ── vault list ─────────────────────────────────────────────────────────────────

fn render_vault_list<O: Write, E: Write>(
    view: VaultListView,
    presenter: &mut Presenter<O, E>,
) -> i32 {
    let format = view.spec.resolve(view.explicit, is_stdout_tty());
    match format {
        Format::Json => list_json(&view.vaults, presenter),
        _ => list_human(&view.vaults, presenter),
    }
}

fn list_human<O: Write, E: Write>(
    vaults: &[RegisteredVault],
    presenter: &mut Presenter<O, E>,
) -> i32 {
    if vaults.is_empty() {
        presenter.diagnostic("no vaults registered");
        return EXIT_OK;
    }
    let (out, err) = presenter.streams();
    let result: io::Result<i32> = (|| {
        for vault in vaults {
            writeln!(
                out,
                "{name}  {root}",
                name = vault.name,
                root = path_display(&vault.root)
            )?;
            for (label, path) in [
                ("config", &vault.config),
                ("cache", &vault.cache),
                ("logs", &vault.logs),
            ] {
                if let Some(path) = path {
                    writeln!(out, "    {label} = {path}", path = path_display(path))?;
                }
            }
        }
        Ok(EXIT_OK)
    })();
    render_outcome(result, err)
}

/// The stable machine shape: an array of objects, one per vault, absent overrides
/// explicit JSON `null` (donor `vault::VaultJson`).
#[derive(Serialize)]
struct VaultJson {
    name: String,
    root: String,
    config: Option<String>,
    cache: Option<String>,
    logs: Option<String>,
}

impl From<&RegisteredVault> for VaultJson {
    fn from(vault: &RegisteredVault) -> Self {
        Self {
            name: vault.name.clone(),
            root: path_display(&vault.root),
            config: vault.config.as_deref().map(path_display),
            cache: vault.cache.as_deref().map(path_display),
            logs: vault.logs.as_deref().map(path_display),
        }
    }
}

fn list_json<O: Write, E: Write>(
    vaults: &[RegisteredVault],
    presenter: &mut Presenter<O, E>,
) -> i32 {
    let rows: Vec<VaultJson> = vaults.iter().map(VaultJson::from).collect();
    match serde_json::to_string_pretty(&rows) {
        Ok(text) => {
            let (out, err) = presenter.streams();
            let result: io::Result<i32> = (|| {
                writeln!(out, "{text}")?;
                Ok(EXIT_OK)
            })();
            render_outcome(result, err)
        }
        Err(source) => {
            presenter.diagnostic(&format!("failed to serialize registry as JSON: {source}"));
            EXIT_OPERATIONAL
        }
    }
}

/// Lossy path→string for display and JSON (donor `vault::display`).
fn path_display(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use norn_wire::{
        DataSummary, DateBounds, DescribeReport, FieldDistribution, GetRecord, GetReport,
        SkippedField, ValueCount,
    };
    use serde_json::json;

    // ── count ────────────────────────────────────────────────────────────

    #[test]
    fn count_total_text_is_padded() {
        let r = CountReport::Total { total: 42 };
        assert_eq!(count_text(&r), "total      42\n");
    }

    #[test]
    fn count_grouped_text_columns_align() {
        let mut groups = BTreeMap::new();
        groups.insert("active".to_string(), 12usize);
        groups.insert("backlog".to_string(), 11usize);
        let r = CountReport::Grouped {
            by: "status".to_string(),
            total: 23,
            groups,
        };
        assert_eq!(
            count_text(&r),
            "total      23\n\nstatus   count\nactive   12\nbacklog  11\n"
        );
    }

    #[test]
    fn count_total_json_is_compact() {
        let r = CountReport::Total { total: 7 };
        assert_eq!(count_json(&r), r#"{"total":7}"#);
    }

    #[test]
    fn count_grouped_json_field_order_is_by_total_groups() {
        let mut groups = BTreeMap::new();
        groups.insert("active".to_string(), 3usize);
        let r = CountReport::Grouped {
            by: "status".to_string(),
            total: 3,
            groups,
        };
        assert_eq!(
            count_json(&r),
            r#"{"by":"status","total":3,"groups":{"active":3}}"#
        );
    }

    // ── describe ─────────────────────────────────────────────────────────

    fn describe_sample() -> DescribeReport {
        DescribeReport {
            folders: vec!["".into(), "notes".into()],
            path_rules: vec![],
            creatable_rules: vec![],
            inbox: None,
            schema: json!({}),
            data: Some(DataSummary {
                total: 1164,
                fields: vec![FieldDistribution {
                    field: "type".into(),
                    values: vec![
                        ValueCount {
                            value: "note".into(),
                            count: 575,
                        },
                        ValueCount {
                            value: "task".into(),
                            count: 420,
                        },
                    ],
                    more: 4,
                }],
                dates: vec![DateBounds {
                    field: "created".into(),
                    min: "2026-05-10".into(),
                    max: "2026-07-03".into(),
                }],
                skipped: vec![SkippedField {
                    field: "title".into(),
                    distinct: 1164,
                    total: 1164,
                }],
            }),
        }
    }

    #[test]
    fn describe_text_renders_structure_then_data() {
        let s = describe_text(&describe_sample());
        assert!(s.contains("folders    2"), "{s}");
        assert!(
            s.contains("1164 documents · created 2026-05-10 → 2026-07-03"),
            "{s}"
        );
        assert!(s.contains("type: note 575 · task 420 (+4 more)"), "{s}");
        assert!(s.contains("(skipped: title 1164/1164)"), "{s}");
    }

    #[test]
    fn describe_structure_only_text_has_no_data_block() {
        let mut report = describe_sample();
        report.data = None;
        assert_eq!(describe_text(&report), "folders    2\n");
    }

    #[test]
    fn describe_json_serializes_the_whole_report() {
        let text = describe_json(&describe_sample());
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["data"]["total"], 1164);
        assert_eq!(v["data"]["fields"][0]["values"][0]["value"], "note");
        assert!(text.starts_with(r#"{"folders":"#), "{text}");
    }

    // ── get records + failure signal ─────────────────────────────────────

    fn get_record(path: &str, fm: Value) -> GetRecord {
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
    fn get_records_default_shows_frontmatter_then_facets() {
        let report = GetReport {
            records: vec![get_record("a.md", json!({"title": "A", "type": "note"}))],
            notes: vec![],
            markdown_content: None,
        };
        let text = render_get_records(&report, &Palette::off(), &[]);
        assert!(text.contains("a.md"), "path header: {text}");
        assert!(text.contains("title"), "frontmatter field: {text}");
        assert!(text.contains("headings"), "headings row: {text}");
    }

    #[test]
    fn get_records_colorize_under_an_enabled_palette() {
        // NRN-362: get honors the resolved palette. Color on → ANSI escapes;
        // off (the piped / parity path) → bytes unchanged.
        let report = GetReport {
            records: vec![get_record("a.md", json!({"title": "A"}))],
            notes: vec![],
            markdown_content: None,
        };
        let colored = render_get_records(&report, &Palette::on(), &[]);
        let plain = render_get_records(&report, &Palette::off(), &[]);
        assert!(
            colored.contains('\u{1b}'),
            "expected ANSI escapes: {colored:?}"
        );
        assert!(
            !plain.contains('\u{1b}'),
            "off palette stays plain: {plain:?}"
        );
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

    // ── render IO-error policy (NRN-372) ───────────────────────────────────
    //
    // One policy, every render path: BrokenPipe is a silent success (the
    // standard `norn find | head` shape); every other IO error is a `norn:
    // <e>` diagnostic plus the operational exit. `FailingWriter` stands in for
    // a stdout/stderr that can't accept another byte (closed pipe, full disk,
    // …), so these prove the policy end-to-end through each render path, not
    // just at the shared helper.

    use crate::cli::{ColorWhen, GlobalArgs};
    use norn_config::RegisteredVault;

    use super::super::format::FormatSpec;

    /// A `Write` that fails every write with a fixed [`io::ErrorKind`].
    struct FailingWriter(io::ErrorKind);

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(self.0))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn global_args() -> GlobalArgs {
        GlobalArgs {
            cwd: None,
            verbose: false,
            no_cache_refresh: false,
            color: ColorWhen::Never,
            vault: None,
            help_short: false,
            help_long: false,
        }
    }

    #[test]
    fn render_outcome_tolerates_broken_pipe_as_success() {
        let mut err = Vec::new();
        let result: io::Result<i32> = Err(io::Error::from(io::ErrorKind::BrokenPipe));
        assert_eq!(render_outcome(result, &mut err), EXIT_OK);
        assert!(err.is_empty(), "broken pipe must not print a diagnostic");
    }

    #[test]
    fn render_outcome_reports_other_io_errors_operationally() {
        let mut err = Vec::new();
        let result: io::Result<i32> = Err(io::Error::other("disk full"));
        assert_eq!(render_outcome(result, &mut err), EXIT_OPERATIONAL);
        assert_eq!(String::from_utf8(err).unwrap(), "norn: disk full\n");
    }

    #[test]
    fn render_outcome_passes_through_the_success_code() {
        let mut err = Vec::new();
        assert_eq!(render_outcome(Ok(EXIT_USAGE), &mut err), EXIT_USAGE);
        assert!(err.is_empty());
    }

    fn one_find_doc() -> FindDoc {
        FindDoc {
            path: "a.md".into(),
            stem: "a".into(),
            hash: "deadbeef".into(),
            frontmatter: None,
            body_text: String::new(),
            headings: vec![],
            outgoing_links: vec![],
            unresolved_links: vec![],
            incoming_links: vec![],
        }
    }

    fn find_view() -> FindView {
        FindView {
            report: FindReport {
                documents: vec![one_find_doc()],
                total: 1,
                returned: 1,
                starts_at: 0,
                truncated: false,
            },
            cols: vec![],
            all_cols: false,
            sort_field: None,
            explicit: Some(Format::Paths),
            spec: FormatSpec {
                tty: Format::Records,
                piped: Format::Paths,
            },
        }
    }

    #[test]
    fn render_find_tolerates_broken_pipe() {
        let mut err = Vec::new();
        let global = global_args();
        let code = {
            let mut presenter = Presenter::new(FailingWriter(io::ErrorKind::BrokenPipe), &mut err);
            render_find(find_view(), &global, &mut presenter)
        };
        assert_eq!(code, EXIT_OK);
        assert!(err.is_empty(), "broken pipe must stay silent: {err:?}");
    }

    #[test]
    fn render_find_reports_other_io_errors() {
        let mut err = Vec::new();
        let global = global_args();
        let code = {
            let mut presenter =
                Presenter::new(FailingWriter(io::ErrorKind::PermissionDenied), &mut err);
            render_find(find_view(), &global, &mut presenter)
        };
        assert_eq!(code, EXIT_OPERATIONAL);
        assert!(
            String::from_utf8(err).unwrap().starts_with("norn: "),
            "expected the norn: diagnostic"
        );
    }

    fn get_view(explicit: Format, report: GetReport) -> GetView {
        GetView {
            report,
            cols: vec![],
            sections: vec![],
            explicit: Some(explicit),
            spec: FormatSpec {
                tty: Format::Records,
                piped: Format::Records,
            },
        }
    }

    #[test]
    fn render_get_tolerates_broken_pipe() {
        let mut err = Vec::new();
        let global = global_args();
        let report = GetReport {
            records: vec![get_record("a.md", json!({"title": "A"}))],
            notes: vec![],
            markdown_content: None,
        };
        let code = {
            let mut presenter = Presenter::new(FailingWriter(io::ErrorKind::BrokenPipe), &mut err);
            render_get(get_view(Format::Paths, report), &global, &mut presenter)
        };
        assert_eq!(code, EXIT_OK);
        assert!(err.is_empty());
    }

    #[test]
    fn render_get_reports_other_io_errors() {
        let mut err = Vec::new();
        let global = global_args();
        let report = GetReport {
            records: vec![get_record("a.md", json!({"title": "A"}))],
            notes: vec![],
            markdown_content: None,
        };
        let code = {
            let mut presenter =
                Presenter::new(FailingWriter(io::ErrorKind::PermissionDenied), &mut err);
            render_get(get_view(Format::Paths, report), &global, &mut presenter)
        };
        assert_eq!(code, EXIT_OPERATIONAL);
        assert!(String::from_utf8(err).unwrap().starts_with("norn: "));
    }

    #[test]
    fn render_get_markdown_tolerates_broken_pipe() {
        let mut err = Vec::new();
        let global = global_args();
        let report = GetReport {
            records: vec![get_record("a.md", json!({}))],
            notes: vec![],
            markdown_content: Some("# hello\n".into()),
        };
        let code = {
            let mut presenter = Presenter::new(FailingWriter(io::ErrorKind::BrokenPipe), &mut err);
            render_get(get_view(Format::Markdown, report), &global, &mut presenter)
        };
        assert_eq!(code, EXIT_OK);
        assert!(err.is_empty());
    }

    #[test]
    fn render_get_markdown_reports_other_io_errors() {
        let mut err = Vec::new();
        let global = global_args();
        let report = GetReport {
            records: vec![get_record("a.md", json!({}))],
            notes: vec![],
            markdown_content: Some("# hello\n".into()),
        };
        let code = {
            let mut presenter =
                Presenter::new(FailingWriter(io::ErrorKind::PermissionDenied), &mut err);
            render_get(get_view(Format::Markdown, report), &global, &mut presenter)
        };
        assert_eq!(code, EXIT_OPERATIONAL);
        assert!(String::from_utf8(err).unwrap().starts_with("norn: "));
    }

    fn count_view(explicit: Format) -> CountView {
        CountView {
            report: CountReport::Total { total: 3 },
            explicit: Some(explicit),
            spec: FormatSpec {
                tty: Format::Records,
                piped: Format::Records,
            },
        }
    }

    #[test]
    fn render_count_tolerates_broken_pipe() {
        let mut err = Vec::new();
        let code = {
            let mut presenter = Presenter::new(FailingWriter(io::ErrorKind::BrokenPipe), &mut err);
            render_count(count_view(Format::Json), &mut presenter)
        };
        assert_eq!(code, EXIT_OK);
        assert!(err.is_empty());
    }

    #[test]
    fn render_count_reports_other_io_errors() {
        let mut err = Vec::new();
        let code = {
            let mut presenter =
                Presenter::new(FailingWriter(io::ErrorKind::PermissionDenied), &mut err);
            render_count(count_view(Format::Json), &mut presenter)
        };
        assert_eq!(code, EXIT_OPERATIONAL);
        assert!(String::from_utf8(err).unwrap().starts_with("norn: "));
    }

    fn describe_view() -> DescribeView {
        DescribeView {
            report: describe_sample(),
            explicit: Some(Format::Json),
            spec: FormatSpec {
                tty: Format::Records,
                piped: Format::Records,
            },
        }
    }

    #[test]
    fn render_describe_tolerates_broken_pipe() {
        let mut err = Vec::new();
        let code = {
            let mut presenter = Presenter::new(FailingWriter(io::ErrorKind::BrokenPipe), &mut err);
            render_describe(describe_view(), &mut presenter)
        };
        assert_eq!(code, EXIT_OK);
        assert!(err.is_empty());
    }

    #[test]
    fn render_describe_reports_other_io_errors() {
        let mut err = Vec::new();
        let code = {
            let mut presenter =
                Presenter::new(FailingWriter(io::ErrorKind::PermissionDenied), &mut err);
            render_describe(describe_view(), &mut presenter)
        };
        assert_eq!(code, EXIT_OPERATIONAL);
        assert!(String::from_utf8(err).unwrap().starts_with("norn: "));
    }

    fn sample_vault() -> RegisteredVault {
        RegisteredVault {
            name: "docs".into(),
            root: std::path::PathBuf::from("/vaults/docs"),
            config: None,
            cache: None,
            logs: None,
        }
    }

    fn vault_list_view(explicit: Format) -> VaultListView {
        VaultListView {
            vaults: vec![sample_vault()],
            explicit: Some(explicit),
            spec: FormatSpec {
                tty: Format::Records,
                piped: Format::Records,
            },
        }
    }

    #[test]
    fn render_vault_list_human_tolerates_broken_pipe() {
        let mut err = Vec::new();
        let code = {
            let mut presenter = Presenter::new(FailingWriter(io::ErrorKind::BrokenPipe), &mut err);
            render_vault_list(vault_list_view(Format::Records), &mut presenter)
        };
        assert_eq!(code, EXIT_OK);
        assert!(err.is_empty());
    }

    #[test]
    fn render_vault_list_human_reports_other_io_errors() {
        let mut err = Vec::new();
        let code = {
            let mut presenter =
                Presenter::new(FailingWriter(io::ErrorKind::PermissionDenied), &mut err);
            render_vault_list(vault_list_view(Format::Records), &mut presenter)
        };
        assert_eq!(code, EXIT_OPERATIONAL);
        assert!(String::from_utf8(err).unwrap().starts_with("norn: "));
    }

    #[test]
    fn render_vault_list_json_tolerates_broken_pipe() {
        let mut err = Vec::new();
        let code = {
            let mut presenter = Presenter::new(FailingWriter(io::ErrorKind::BrokenPipe), &mut err);
            render_vault_list(vault_list_view(Format::Json), &mut presenter)
        };
        assert_eq!(code, EXIT_OK);
        assert!(err.is_empty());
    }

    #[test]
    fn render_vault_list_json_reports_other_io_errors() {
        let mut err = Vec::new();
        let code = {
            let mut presenter =
                Presenter::new(FailingWriter(io::ErrorKind::PermissionDenied), &mut err);
            render_vault_list(vault_list_view(Format::Json), &mut presenter)
        };
        assert_eq!(code, EXIT_OPERATIONAL);
        assert!(String::from_utf8(err).unwrap().starts_with("norn: "));
    }

    #[test]
    fn emit_line_tolerates_broken_pipe() {
        let mut err = Vec::new();
        let global = global_args();
        let code = {
            let mut presenter = Presenter::new(FailingWriter(io::ErrorKind::BrokenPipe), &mut err);
            emit(Ok(Output::Line("ok".into())), &global, &mut presenter)
        };
        assert_eq!(code, EXIT_OK);
        assert!(err.is_empty());
    }

    #[test]
    fn emit_line_reports_other_io_errors() {
        let mut err = Vec::new();
        let global = global_args();
        let code = {
            let mut presenter =
                Presenter::new(FailingWriter(io::ErrorKind::PermissionDenied), &mut err);
            emit(Ok(Output::Line("ok".into())), &global, &mut presenter)
        };
        assert_eq!(code, EXIT_OPERATIONAL);
        assert!(String::from_utf8(err).unwrap().starts_with("norn: "));
    }

    #[test]
    fn emit_usage_tolerates_broken_pipe_on_stderr() {
        let global = global_args();
        let code = {
            let mut presenter =
                Presenter::new(Vec::new(), FailingWriter(io::ErrorKind::BrokenPipe));
            emit(
                Ok(Output::Usage(b"usage text".to_vec())),
                &global,
                &mut presenter,
            )
        };
        assert_eq!(code, EXIT_OK);
    }

    #[test]
    fn emit_usage_reports_other_io_errors_on_stderr() {
        // Usage text writes to stderr; a genuine IO failure there still needs
        // the norn: diagnostic and the operational exit — sharing stderr with
        // the diagnostic path doesn't exempt Usage from the policy.
        let global = global_args();
        let code = {
            let mut presenter =
                Presenter::new(Vec::new(), FailingWriter(io::ErrorKind::PermissionDenied));
            emit(
                Ok(Output::Usage(b"usage text".to_vec())),
                &global,
                &mut presenter,
            )
        };
        assert_eq!(code, EXIT_OPERATIONAL);
    }
}
