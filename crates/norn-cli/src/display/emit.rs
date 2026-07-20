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
    CountReport, DescribeReport, FindDoc, FindReport, FrontmatterChange, GetRecord, GetReport,
    GroupNode, MutationOutcome, MutationWarning,
};
use serde::Serialize;
use serde_json::Value;

use crate::cli::GlobalArgs;
use crate::output::glyphs::{self, Glyph};
use crate::output::palette::{self, Palette};
use crate::output::primitives::{self, Field};
use crate::output::projection::{
    project_json, project_pairs, split_cols, unknown_facet_message, warn_col_ignored,
    warn_section_ignored, DefaultCols, DocView, KNOWN_FACETS,
};

use super::conversation::Conversation;
use super::fix_hints::fix_hint_for;
use super::format::Format;
use super::output::{
    CountView, DeleteMutationView, DescribeView, EditMutationView, FindView, GetView,
    MoveMutationView, NewMutationView, Output, RepairView, RewriteWikilinkView, SetMutationView,
    ValidateView, VaultListView,
};
use super::sink::Sink;
use super::{Diagnostic, Presenter, EXIT_OK, EXIT_OPERATIONAL, EXIT_USAGE};
use crate::cli::RepairPlanFormat;
use norn_core::apply::report::{ApplyOutcome, ApplyReport};
use norn_core::plan::MigrationPlan;

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
        Output::Validate(view) => render_validate(view, global, presenter),
        Output::Repair(view) => render_repair(view, global, presenter),
        Output::VaultList(view) => render_vault_list(view, presenter),
        Output::Set(view) => render_set(view, global, presenter),
        Output::New(view) => render_new(view, global, presenter),
        Output::Edit(view) => render_edit(view, global, presenter),
        Output::Move(view) => render_move(view, presenter),
        Output::Delete(view) => render_delete(view, presenter),
        Output::RewriteWikilink(view) => render_rewrite_wikilink(view, presenter),
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
        warn_unknown_sort_find(&view.report, view.sort_field.as_deref(), conv.writer())?;
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

/// Warn when `--sort FIELD` names a frontmatter key absent from every
/// RETURNED document — the query still runs (every row's `json_extract`
/// misses, so the SQL ORDER BY falls through to its `path ASC` tiebreak —
/// effectively unsorted), matching the `--col` "not present in any matching
/// document" precedent structurally (NRN-374, the still-unplumbed surface
/// flagged in `FindParams`/`CountParams::dynamic_keys`). `path`/`stem` are the
/// two virtual/structural sort keys (never frontmatter) and are exempt.
///
/// Guarded to the FULL result set (never a truncated page): checking only
/// `report.documents` (the returned page, after `--limit`) would false-positive
/// on a field that is real but happens to live only beyond the page boundary —
/// with NULLs-first ordering, an unsorted-by-this-field sparse field is exactly
/// as likely to be absent from page 1 as present. So this returns early
/// whenever `report.truncated` (`returned < total`), and likewise on a
/// zero-match result: every field would trivially "not be present" in an empty
/// set, which is redundant with the `total: 0` signal rather than informative
/// about the field itself. (The pre-existing `--col` warning has the identical
/// truncation false positive — same oracle-parity-locked behavior on both
/// sides, so it is intentionally left as-is here; a fix there is a banked
/// ledger candidate, not part of this new-surface warning.)
fn warn_unknown_sort_find(
    report: &FindReport,
    sort_field: Option<&str>,
    err: &mut dyn Write,
) -> io::Result<()> {
    let Some(field) = sort_field else {
        return Ok(());
    };
    if matches!(field, "path" | "stem") || report.documents.is_empty() || report.truncated {
        return Ok(());
    }
    let present_in_any = report.documents.iter().any(|d| {
        d.frontmatter
            .as_ref()
            .and_then(|fm| fm.as_object())
            .is_some_and(|obj| obj.contains_key(field))
    });
    if !present_in_any {
        writeln!(
            err,
            "warning: --sort field `{field}` not present in any matching document"
        )?;
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
        warn_unknown_sort_get(&view.report, view.sort_field.as_deref(), conv.writer())?;
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

/// `get`'s counterpart to `warn_unknown_sort_find` (NRN-374): same field-absent
/// check over the resolved records, same `path`/`stem` exemption and zero-match
/// skip, but the `warn:` prefix matching `get`'s own `--col` convention above.
fn warn_unknown_sort_get(
    report: &GetReport,
    sort_field: Option<&str>,
    err: &mut dyn Write,
) -> io::Result<()> {
    let Some(field) = sort_field else {
        return Ok(());
    };
    if matches!(field, "path" | "stem") || report.records.is_empty() {
        return Ok(());
    }
    let present_in_any = report.records.iter().any(|r| {
        r.frontmatter
            .as_ref()
            .and_then(Value::as_object)
            .is_some_and(|obj| obj.contains_key(field))
    });
    if !present_in_any {
        writeln!(err, "warn: --sort field '{field}' not present in document")?;
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
        warn_unknown_by_count(&view.report, err)?;
        Ok(EXIT_OK)
    })();
    render_outcome(result, err)
}

/// The wire's "field entirely absent" bucket value (`"(missing)"`, mirrored
/// here rather than imported since `norn_core::read::MISSING` is crate-private
/// — it is already part of the stable `count`/`describe --data` JSON contract).
const MISSING_BUCKET: &str = "(missing)";

/// Warn when a `--by` field groups EVERY matched document into the
/// `(missing)` bucket — the count still runs, matching the `--col` "not
/// present in any matching document" precedent structurally (NRN-374, the
/// still-unplumbed surface flagged in `CountParams::dynamic_keys`). Only the
/// OUTERMOST `--by` field is checked for a multi-field group tree — a nested
/// field's presence would need a per-branch walk, out of scope here (a
/// documented simplification, not a correctness bug: the outermost field is
/// the common case and the one the donor's `--count-by` precedent covered).
/// A zero-match count naturally produces an EMPTY `groups` map (not an
/// all-`(missing)` one), so this never fires spuriously on `total: 0`.
fn warn_unknown_by_count(report: &CountReport, err: &mut dyn Write) -> io::Result<()> {
    let (field, all_missing) = match report {
        CountReport::Total { .. } => return Ok(()),
        CountReport::Grouped { by, groups, .. } => (
            by.as_str(),
            groups.len() == 1 && groups.contains_key(MISSING_BUCKET),
        ),
        CountReport::GroupedMulti { by, groups, .. } => {
            let Some(first) = by.first() else {
                return Ok(());
            };
            (
                first.as_str(),
                groups.len() == 1 && groups.contains_key(MISSING_BUCKET),
            )
        }
    };
    if all_missing {
        writeln!(
            err,
            "warning: --by field `{field}` not present in any matching document"
        )?;
    }
    Ok(())
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
        warn_unknown_by_describe(&view.report, &view.by, err)?;
        Ok(EXIT_OK)
    })();
    render_outcome(result, err)
}

/// `describe`'s counterpart to `warn_unknown_by_count` (NRN-374). Unlike
/// `count`, describe's `field_distributions` (`norn-core`) drops a `--by`
/// field ENTIRELY from `data.fields` when it has zero occurrences across the
/// matched set (no `(missing)` bucket is even synthesized for an explicit
/// `--by`, unlike `count`'s always-present bucket) — so "absent" here means
/// "requested but not in `data.fields`" rather than an all-`(missing)` bucket.
/// `by` is trimmed the same way `describe::execute`'s internal `normalize_by`
/// does (never de-duped, matching it), so a comma/whitespace-only entry never
/// warns. Skipped entirely on `data: None` (no `--data`/`--by` requested) and
/// on a zero-match `data.total` (every field would trivially be absent,
/// redundant with the `0 documents` line).
fn warn_unknown_by_describe(
    report: &DescribeReport,
    by: &[String],
    err: &mut dyn Write,
) -> io::Result<()> {
    let Some(data) = &report.data else {
        return Ok(());
    };
    if data.total == 0 {
        return Ok(());
    }
    for field in by.iter().map(|f| f.trim()).filter(|f| !f.is_empty()) {
        let present = data.fields.iter().any(|fd| fd.field == field);
        if !present {
            writeln!(
                err,
                "warning: --by field `{field}` not present in any matching document"
            )?;
        }
    }
    Ok(())
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

// ── validate ─────────────────────────────────────────────────────────────────

fn render_validate<O: Write, E: Write>(
    view: ValidateView,
    global: &GlobalArgs,
    presenter: &mut Presenter<O, E>,
) -> i32 {
    let format = view.spec.resolve(view.explicit, is_stdout_tty());
    let palette = palette::resolve(global.color);
    let width = term_width();

    // Parse each carried finding line (compact struct-order JSON) back to a
    // Value for the json / records / paths projections; jsonl writes the lines
    // verbatim (so its per-line bytes stay the donor's struct field order).
    let findings: Vec<Value> = view
        .report
        .findings
        .iter()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();

    let (out, err) = presenter.streams();
    let result: io::Result<i32> = (|| {
        match format {
            Format::Json => {
                if view.summary {
                    // `summary_json` is pretty-printed core-side (Some whenever
                    // `--summary` was set — the case that reaches here); emit it
                    // with exactly one trailing newline.
                    let body = view.report.summary_json.as_deref().unwrap_or("{}");
                    writeln!(out, "{}", body.trim_end_matches('\n'))?;
                } else {
                    let payload = serde_json::json!({
                        "total": findings.len(),
                        "findings": findings,
                    });
                    writeln!(out, "{}", serde_json::to_string_pretty(&payload)?)?;
                }
            }
            Format::Jsonl => {
                for line in &view.report.findings {
                    writeln!(out, "{line}")?;
                }
            }
            Format::Paths => {
                let paths: std::collections::BTreeSet<&str> =
                    findings.iter().filter_map(|f| f["path"].as_str()).collect();
                for path in paths {
                    writeln!(out, "{path}")?;
                }
            }
            Format::Records => {
                let mut buf: Vec<u8> = Vec::new();
                if view.summary {
                    render_validate_summary(
                        &mut buf,
                        &palette,
                        &findings,
                        view.report.rules_count,
                        view.report.total_docs,
                        width,
                    )?;
                } else {
                    render_validate_full(
                        &mut buf,
                        &palette,
                        &findings,
                        view.report.rules_count,
                        view.report.total_docs,
                    )?;
                }
                out.write_all(&buf)?;
            }
            Format::Markdown => unreachable!("validate has no markdown format"),
        }
        Ok(if view.report.has_errors {
            EXIT_OPERATIONAL
        } else {
            EXIT_OK
        })
    })();

    render_outcome(result, err)
}

/// Count warning / error findings by their serialized `severity` field.
fn count_severities(findings: &[Value]) -> (usize, usize) {
    let mut warn = 0;
    let mut err = 0;
    for f in findings {
        match f["severity"].as_str() {
            Some("error") => err += 1,
            _ => warn += 1,
        }
    }
    (warn, err)
}

/// Distinct affected-document count (for the pass tally).
fn unique_doc_count(findings: &[Value]) -> usize {
    findings
        .iter()
        .filter_map(|f| f["path"].as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len()
}

/// `--summary` records: status headline, severity tally, and (when non-empty) a
/// by-code tally group. Donor `render_records_summary`.
fn render_validate_summary(
    out: &mut dyn Write,
    palette: &Palette,
    findings: &[Value],
    rules_count: usize,
    total_docs: usize,
    width: usize,
) -> io::Result<()> {
    let ascii = glyphs::use_ascii();
    primitives::status_headline(
        out,
        palette,
        &format!("running {rules_count} rules across {total_docs} documents"),
        ascii,
    )?;
    writeln!(out)?;

    let (warn, err) = count_severities(findings);
    let pass = total_docs.saturating_sub(unique_doc_count(findings));
    primitives::severity_tally(out, palette, pass, warn, err, "documents")?;

    if !findings.is_empty() {
        writeln!(out)?;
        // by-code counts, sorted by code (donor `summarize().codes`, a BTreeMap).
        let mut by_code: std::collections::BTreeMap<&str, usize> =
            std::collections::BTreeMap::new();
        for f in findings {
            if let Some(code) = f["code"].as_str() {
                *by_code.entry(code).or_insert(0) += 1;
            }
        }
        let rows: Vec<(&str, usize)> = by_code.into_iter().collect();
        primitives::tally_group(out, palette, "by code", &rows, width, ascii)?;
    }
    Ok(())
}

/// Full records: status headline; then per-code groups (first-occurrence order)
/// with a severity glyph header, each finding's path + message + optional fix
/// hint; then a pass/shown footer. Donor `render_records_full`.
fn render_validate_full(
    out: &mut dyn Write,
    palette: &Palette,
    findings: &[Value],
    rules_count: usize,
    total_docs: usize,
) -> io::Result<()> {
    let ascii = glyphs::use_ascii();
    primitives::status_headline(
        out,
        palette,
        &format!("running {rules_count} rules across {total_docs} documents"),
        ascii,
    )?;

    if findings.is_empty() {
        writeln!(out)?;
        primitives::severity_tally(out, palette, total_docs, 0, 0, "documents")?;
        return Ok(());
    }

    for (code, group) in group_by_code(findings) {
        writeln!(out)?;
        let is_error = group
            .first()
            .and_then(|f| f["severity"].as_str())
            .is_some_and(|s| s == "error");
        let (glyph, style) = if is_error {
            (glyphs::render(Glyph::Err, ascii), &palette.rune)
        } else {
            (glyphs::render(Glyph::Warn, ascii), &palette.amber)
        };
        writeln!(
            out,
            "{}{glyph}{} {}{code}{}",
            style.render(),
            style.render_reset(),
            palette.bone.render(),
            palette.bone.render_reset(),
        )?;
        for f in group {
            writeln!(
                out,
                "  {}{}{}",
                palette.bone.render(),
                f["path"].as_str().unwrap_or(""),
                palette.bone.render_reset(),
            )?;
            writeln!(
                out,
                "    {}{}{}",
                palette.dim.render(),
                f["message"].as_str().unwrap_or(""),
                palette.dim.render_reset(),
            )?;
            if let Some(hint) = fix_hint_for(code) {
                writeln!(
                    out,
                    "    {}fix:{} {}{hint}{}",
                    palette.thread.render(),
                    palette.thread.render_reset(),
                    palette.dim.render(),
                    palette.dim.render_reset(),
                )?;
            }
        }
    }

    writeln!(out)?;
    let pass = total_docs.saturating_sub(unique_doc_count(findings));
    let sep = glyphs::render(Glyph::Sep, ascii);
    writeln!(
        out,
        "{}{pass} documents pass {sep} {} findings shown{}",
        palette.dim.render(),
        findings.len(),
        palette.dim.render_reset(),
    )?;
    Ok(())
}

/// Group findings by `code`, preserving first-occurrence code order (donor
/// `group_by_code`).
fn group_by_code(findings: &[Value]) -> Vec<(&str, Vec<&Value>)> {
    let mut order: Vec<&str> = Vec::new();
    let mut map: std::collections::BTreeMap<&str, Vec<&Value>> = std::collections::BTreeMap::new();
    for f in findings {
        let code = f["code"].as_str().unwrap_or("");
        if !map.contains_key(code) {
            order.push(code);
        }
        map.entry(code).or_default().push(f);
    }
    order
        .into_iter()
        .map(|code| {
            let group = map.remove(code).unwrap_or_default();
            (code, group)
        })
        .collect()
}

// ── repair (findings → plan; read-only) ──────────────────────────────────────

/// `repair`: bare `norn repair` prints the findings summary; `--plan` emits the
/// `MigrationPlan` (report / json / paths) and/or writes it to `--out`. The exit
/// code is `has_diagnostic_errors` for both — the donor's `exit_code_for`,
/// independent of the triage filters (a `--code` narrow never masks a vault
/// error). Byte-faithful to the donor `repair::{run_summary, emit_plan, render}`.
fn render_repair<O: Write, E: Write>(
    view: RepairView,
    global: &GlobalArgs,
    presenter: &mut Presenter<O, E>,
) -> i32 {
    let palette = palette::resolve(global.color);
    let width = term_width();
    let ascii = glyphs::use_ascii();

    let (out, err) = presenter.streams();

    // The owner produced `plan_json` via `to_string_pretty` of a valid
    // MigrationPlan, so this parse cannot realistically fail; a corrupt frame is
    // an operational failure, not a panic.
    let plan: MigrationPlan = match serde_json::from_str(&view.report.plan_json) {
        Ok(p) => p,
        Err(e) => {
            let _ = writeln!(err, "norn: undecodable repair plan: {e}");
            return EXIT_OPERATIONAL;
        }
    };

    let result: io::Result<i32> = (|| {
        if view.plan {
            emit_repair_plan(out, &palette, &plan, &view, width, ascii)?;
        } else {
            write_repair_summary(out, &view.report, &plan)?;
        }
        Ok(if view.report.has_diagnostic_errors {
            EXIT_OPERATIONAL
        } else {
            EXIT_OK
        })
    })();

    render_outcome(result, err)
}

/// Bare `norn repair`: a read-only findings summary — total findings across the
/// vault, a per-code tally, and how many the planner would turn into operations
/// vs. skip. Donor `repair::run_summary`.
fn write_repair_summary(
    out: &mut dyn Write,
    report: &norn_wire::RepairReport,
    plan: &MigrationPlan,
) -> io::Result<()> {
    writeln!(
        out,
        "{} findings across {} documents",
        report.findings_total, report.total_docs
    )?;
    for (code, count) in &report.findings_by_code {
        writeln!(out, "  {count}  {code}")?;
    }
    writeln!(out)?;
    let ops = plan.operations.len();
    let skipped = plan.skipped.len();
    writeln!(out, "{ops} repairable as operations, {skipped} skipped")?;
    if ops > 0 || skipped > 0 {
        writeln!(out)?;
        writeln!(out, "Run `norn repair --plan` to generate a MigrationPlan.")?;
        writeln!(out, "Pipe it into `norn apply -` to apply.")?;
    }
    Ok(())
}

/// `norn repair --plan`: `--out` writes the JSON plan to a file (independent of
/// `--format`); `--format` governs stdout (report / json / paths), silent when
/// `--out` is set without `--format`. Donor `repair::emit_plan`.
fn emit_repair_plan(
    out: &mut dyn Write,
    palette: &Palette,
    plan: &MigrationPlan,
    view: &RepairView,
    width: usize,
    ascii: bool,
) -> io::Result<()> {
    // --out: always writes JSON (the pretty plan bytes the owner already
    // produced), independent of --format.
    if let Some(path) = &view.out {
        std::fs::write(path, format!("{}\n", view.report.plan_json))?;
    }

    let stdout_format = if view.format.is_none() && view.out.is_some() {
        None
    } else {
        Some(view.format.unwrap_or_else(|| {
            if is_stdout_tty() {
                RepairPlanFormat::Report
            } else {
                RepairPlanFormat::Json
            }
        }))
    };

    match stdout_format {
        Some(RepairPlanFormat::Report) => {
            write_repair_report(out, palette, plan, &view.filter_flags, width, ascii)?
        }
        Some(RepairPlanFormat::Json) => {
            // The owner's `to_string_pretty` bytes, verbatim, with one newline.
            writeln!(out, "{}", view.report.plan_json)?;
        }
        Some(RepairPlanFormat::Paths) => write_repair_paths(out, plan)?,
        None => {}
    }
    Ok(())
}

/// The vault-relative paths a single op touches — frontmatter/link ops carry
/// `path`; structural moves carry `src`/`dst`. Donor `render::op_paths`.
fn repair_op_paths(op: &norn_core::plan::MigrationOp) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(obj) = op.fields.as_object() {
        for key in ["path", "src", "dst", "destination"] {
            if let Some(v) = obj.get(key).and_then(|v| v.as_str()) {
                paths.push(v.to_string());
            }
        }
    }
    paths
}

/// `--format paths`: every affected vault-relative path, sorted + deduplicated,
/// one per line. Donor `render::write_paths`.
fn write_repair_paths(out: &mut dyn Write, plan: &MigrationPlan) -> io::Result<()> {
    let paths: std::collections::BTreeSet<String> =
        plan.operations.iter().flat_map(repair_op_paths).collect();
    for path in paths {
        writeln!(out, "{path}")?;
    }
    Ok(())
}

/// `--format report`: the human decision-support summary — a headline, an
/// operations-by-kind tally, footnotes, a skipped-by-reason tally, top affected
/// files, and filter-aware apply guidance. Donor `render::write_report`.
fn write_repair_report(
    out: &mut dyn Write,
    palette: &Palette,
    plan: &MigrationPlan,
    filter_flags: &[String],
    width: usize,
    ascii: bool,
) -> io::Result<()> {
    use std::collections::{BTreeMap, BTreeSet};

    primitives::status_headline(
        out,
        palette,
        &format!("Repair plan against {}", plan.vault_root),
        ascii,
    )?;
    writeln!(out)?;

    let n_ops = plan.operations.len();
    let n_files: usize = plan
        .operations
        .iter()
        .flat_map(repair_op_paths)
        .collect::<BTreeSet<_>>()
        .len();
    writeln!(out, "  {n_ops} operations proposed across {n_files} files")?;

    if n_ops > 0 {
        let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
        for op in &plan.operations {
            *by_kind.entry(op.kind.as_str()).or_insert(0) += 1;
        }
        let rows: Vec<(&str, usize)> = by_kind.into_iter().collect();
        primitives::tally_group(out, palette, "Operations by kind", &rows, width, ascii)?;
    }
    writeln!(out)?;

    let footnotes: Vec<&String> = plan
        .operations
        .iter()
        .filter_map(|op| op.footnote.as_ref())
        .collect();
    if !footnotes.is_empty() {
        primitives::status_headline(
            out,
            palette,
            &format!("Footnotes ({})", footnotes.len()),
            ascii,
        )?;
        for note in &footnotes {
            writeln!(out, "  {note}")?;
        }
        writeln!(out)?;
    }

    if !plan.skipped.is_empty() {
        let mut by_reason: BTreeMap<&str, usize> = BTreeMap::new();
        for sf in &plan.skipped {
            *by_reason.entry(sf.reason.as_str()).or_insert(0) += 1;
        }
        let labels: Vec<(String, usize)> = by_reason
            .iter()
            .map(|(code, &count)| (format!("{code}  {}", skip_reason_prose(code)), count))
            .collect();
        let rows: Vec<(&str, usize)> = labels.iter().map(|(l, c)| (l.as_str(), *c)).collect();
        primitives::tally_group(
            out,
            palette,
            &format!("Skipped ({})", plan.skipped.len()),
            &rows,
            width,
            ascii,
        )?;
        writeln!(out)?;
    }

    const TOP_FILES_N: usize = 5;
    if n_ops > 0 {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for op in &plan.operations {
            for path in repair_op_paths(op) {
                *counts.entry(path).or_insert(0) += 1;
            }
        }
        if !counts.is_empty() {
            let mut sorted: Vec<(String, usize)> = counts.into_iter().collect();
            sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            sorted.truncate(TOP_FILES_N);
            let rows: Vec<(&str, usize)> =
                sorted.iter().map(|(l, c)| (l.as_str(), *c)).collect();
            primitives::tally_group(out, palette, "Top affected files", &rows, width, ascii)?;
            writeln!(out)?;
        }
    }

    // Apply guidance — filter-aware (donor).
    let confidence_active = filter_flags.iter().any(|f| f == "--confidence");
    let skip_reason_active = filter_flags.iter().any(|f| f == "--skip-reason");
    let has_actionable = n_ops > 0 || !plan.skipped.is_empty();
    if has_actionable {
        writeln!(out, "  To inspect proposed changes")?;
        if !confidence_active {
            let mut high = filter_flags.to_vec();
            high.push("--confidence".into());
            high.push("high".into());
            writeln!(out, "    {}", repair_command(&high, &["--format", "json"]))?;
        }
        writeln!(
            out,
            "    {}",
            repair_command(filter_flags, &["--format", "json"])
        )?;
        writeln!(out)?;
    }
    if !skip_reason_active && n_ops > 0 {
        let apply_flags = if confidence_active {
            filter_flags.to_vec()
        } else {
            let mut v = filter_flags.to_vec();
            v.push("--confidence".into());
            v.push("high".into());
            v
        };
        writeln!(out, "  To apply")?;
        writeln!(
            out,
            "    {} | norn apply -",
            repair_command(&apply_flags, &["--format", "json"])
        )?;
        writeln!(out)?;
    }

    Ok(())
}

/// A `norn repair --plan <flags> <trailing>` command string for the report's
/// apply-guidance lines. Donor `render::build_command`.
fn repair_command(filter_flags: &[String], trailing: &[&str]) -> String {
    let mut parts: Vec<String> = vec!["norn".into(), "repair".into(), "--plan".into()];
    parts.extend(filter_flags.iter().cloned());
    parts.extend(trailing.iter().map(|s| s.to_string()));
    parts.join(" ")
}

/// User-facing prose for a stable skip-reason code (donor
/// `repair::skip_reasons::prose_for`). One point of evolution: a new
/// `SkipReason::code()` variant adds a matching arm here.
fn skip_reason_prose(code: &str) -> &'static str {
    match code {
        "missing-default" => "missing field has no configured deterministic default",
        "link-decision-needed" => "link repair requires an explicit path/link decision",
        "no-rule-matched" => "no configured deterministic repair rule matched",
        "alias-shadowed" => "alias shadowed by a doc stem cannot be repaired deterministically",
        "graph-diagnostic" => "graph diagnostic cannot be repaired deterministically",
        "ambiguous-target" => "ambiguous link target",
        "missing-hash" => "index missing hash for finding's path",
        "precondition-failed" => "rule precondition blocked producing a change",
        _ => "(unknown skip reason)",
    }
}

// ── set / new (mutation reports) ─────────────────────────────────────────────

/// Render a value for a change line: a bare string prints unquoted (`draft`),
/// every other JSON value prints its compact JSON (`3`, `["a","b"]`, `null`) —
/// the donor `value_repr`.
fn value_repr(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The exit code a mutation report implies: a clean pre-write decline
/// (`outcome = refused`) is exit 2 (the refusal is authoritative, nothing
/// happened); a forecast or applied report is exit 0.
fn mutation_exit(outcome: MutationOutcome) -> i32 {
    match outcome {
        MutationOutcome::Refused => EXIT_USAGE,
        MutationOutcome::Applied => EXIT_OK,
    }
}

/// Render the non-fatal mutation warnings (count + the first three messages, with
/// a `… (N more)` tail) — the donor truncation, on the stderr conversation.
/// The mutation warning's records short form — the donor `warning_label`
/// vocabulary, computed per `code` from the unified `{ code, field, message }`
/// envelope (the JSON shape is a deliberate divergence; see
/// `norn_wire::MutationWarning`). Kinds whose records line differs from the
/// message (`unknown-field`, `force-bypass`, `title-ignored`) are rebuilt from
/// `code` + `field`; the rest print their `message` verbatim (it already equals
/// the donor label — e.g. the wikilink warnings).
fn warning_short(w: &MutationWarning) -> String {
    let field = w.field.as_deref().unwrap_or("");
    match w.code.as_str() {
        "unknown-field" => format!("unknown field: {field}"),
        "force-bypass" => format!("--force bypass: {field}"),
        "title-ignored" => format!("title-ignored: {}", w.message),
        _ => w.message.clone(),
    }
}

fn render_set<O: Write, E: Write>(
    view: SetMutationView,
    global: &GlobalArgs,
    presenter: &mut Presenter<O, E>,
) -> i32 {
    let format = view.spec.resolve(view.explicit, is_stdout_tty());
    let report = &view.report;
    let (out, err) = presenter.streams();
    let mut conv = Conversation::new(err);

    // JSON: the compact whole-report serialization is the contract (struct field
    // order, donor-faithful). Structured on refusal too (ADR 0016 unifies the
    // surfaces on the structured envelope).
    if format == Format::Json {
        let result: io::Result<i32> = (|| {
            writeln!(out, "{}", serde_json::to_string(report)?)?;
            Ok(mutation_exit(report.outcome))
        })();
        return render_outcome(result, conv.writer());
    }

    // Records. A refusal prints the coded error on stderr (exit 2).
    if report.outcome == MutationOutcome::Refused {
        let msg = report
            .error
            .as_ref()
            .map(|e| e.message.clone())
            .unwrap_or_else(|| "set refused".to_string());
        let result: io::Result<i32> = (|| {
            conv.line(&format!("error: {msg}"))?;
            Ok(EXIT_USAGE)
        })();
        return render_outcome(result, conv.writer());
    }

    let palette = palette::resolve(global.color);
    let ascii = glyphs::use_ascii();
    let result: io::Result<i32> = (|| {
        let verb = if report.applied {
            "set"
        } else {
            "dry-run: set"
        };
        writeln!(
            out,
            "{}{verb} {}{}",
            palette.header.render(),
            report.target,
            palette.header.render_reset()
        )?;
        for change in &report.frontmatter_changes {
            render_frontmatter_change(out, &palette, change, ascii)?;
        }
        if report.body_changed {
            let old = report.body_bytes_old.unwrap_or(0);
            let new = report.body_bytes_new.unwrap_or(0);
            primitives::change_line(
                out,
                &palette,
                "body",
                &format!("{old} bytes"),
                Some(&format!("{new} bytes")),
                ascii,
            )?;
        }
        // Warnings block (donor: on STDOUT): `  warnings: N` then `    - <short>`
        // for the first three, then `    … (K more)`.
        if !report.warnings.is_empty() {
            writeln!(out, "  warnings: {}", report.warnings.len())?;
            for w in report.warnings.iter().take(3) {
                writeln!(out, "    - {}", warning_short(w))?;
            }
            if report.warnings.len() > 3 {
                writeln!(out, "    … ({} more)", report.warnings.len() - 3)?;
            }
        }
        if report.applied {
            writeln!(out, "trace: {}", report.trace_id)?;
        } else {
            writeln!(out)?;
            writeln!(out, "Apply with --yes")?;
        }
        Ok(EXIT_OK)
    })();
    render_outcome(result, conv.writer())
}

/// One `set` change line, dispatched by the normalized op. An absent prior value
/// (a field added by an upsert) renders its `before` as `<none>` (donor).
fn render_frontmatter_change(
    out: &mut dyn Write,
    palette: &Palette,
    change: &FrontmatterChange,
    ascii: bool,
) -> io::Result<()> {
    let field = change.field.as_str();
    match change.op.as_str() {
        "set" => {
            let before = change
                .old
                .as_ref()
                .map(value_repr)
                .unwrap_or_else(|| "<none>".to_string());
            let after = change.new.as_ref().map(value_repr).unwrap_or_default();
            primitives::change_line(out, palette, field, &before, Some(&after), ascii)
        }
        "remove" => {
            let was = change.old.as_ref().map(value_repr).unwrap_or_default();
            primitives::change_line(
                out,
                palette,
                field,
                &format!("remove (was {was})"),
                None,
                ascii,
            )
        }
        "push" => {
            let v = change.value.as_ref().map(value_repr).unwrap_or_default();
            primitives::change_line(out, palette, field, &format!("push {v}"), None, ascii)
        }
        "pop" => {
            let v = change.value.as_ref().map(value_repr).unwrap_or_default();
            let found = change.found.unwrap_or(false);
            primitives::change_line(
                out,
                palette,
                field,
                &format!("pop {v} (found: {found})"),
                None,
                ascii,
            )
        }
        other => primitives::change_line(out, palette, field, other, None, ascii),
    }
}

/// A `new`-report key/value line: `{label:<9}  {value}` (value column at 11), the
/// donor's aligned block. Unstyled (like `count` / `describe`) so the piped /
/// parity bytes are exact.
fn new_kv(out: &mut dyn Write, label: &str, value: &str) -> io::Result<()> {
    writeln!(out, "{label:<9}  {value}")
}

/// The provenance detail for a created field (donor `report.rs`): the source
/// label as the core already set it (`operator-flag` / `operator-flag-json` /
/// `schema-default` — one vocabulary, no remap), plus the crediting rule when a
/// default carried one: `schema-default, task-rule`.
fn created_detail(created: &norn_wire::FrontmatterCreated) -> String {
    match &created.rule {
        Some(rule) => format!("{}, {}", created.source, rule),
        None => created.source.clone(),
    }
}

fn render_new<O: Write, E: Write>(
    view: NewMutationView,
    global: &GlobalArgs,
    presenter: &mut Presenter<O, E>,
) -> i32 {
    let format = view.spec.resolve(view.explicit, is_stdout_tty());
    let report = &view.report;
    let (out, err) = presenter.streams();
    let mut conv = Conversation::new(err);

    // JSON: pretty-printed with ALPHABETICAL keys (the donor serialized through a
    // `serde_json::Value`, whose map is a BTreeMap without `preserve_order`).
    if format == Format::Json {
        let _ = global;
        let result: io::Result<i32> = (|| {
            let value = serde_json::to_value(report)?;
            // The donor's `new --format json` emits NO trailing newline (unlike
            // `set --format json`, which does) — a donor inconsistency mirrored
            // faithfully with `write!`, not `writeln!`.
            write!(out, "{}", serde_json::to_string_pretty(&value)?)?;
            Ok(mutation_exit(report.outcome))
        })();
        return render_outcome(result, conv.writer());
    }

    if report.outcome == MutationOutcome::Refused {
        let msg = report
            .error
            .as_ref()
            .map(|e| e.message.clone())
            .unwrap_or_else(|| "new refused".to_string());
        let result: io::Result<i32> = (|| {
            conv.line(&format!("error: {msg}"))?;
            Ok(EXIT_USAGE)
        })();
        return render_outcome(result, conv.writer());
    }

    let result: io::Result<i32> = (|| {
        let shown_path = report
            .path
            .as_deref()
            .or(report.predicted_path.as_deref())
            .unwrap_or("(pending)");
        new_kv(out, "path", shown_path)?;
        new_kv(out, "operation", "new")?;
        new_kv(out, "applied", &report.applied.to_string())?;

        // fields: `none`, or the first created field on the `fields` line with the
        // rest aligned under the value column (11-space continuation indent).
        if report.frontmatter_created.is_empty() {
            new_kv(out, "fields", "none")?;
        } else {
            // Field-name sub-column padding (donor report.rs max_field_w): every
            // `field` cell is left-padded to the widest name so the `=` aligns.
            let field_w = report
                .frontmatter_created
                .iter()
                .map(|c| c.field.len())
                .max()
                .unwrap_or(0);
            for (i, created) in report.frontmatter_created.iter().enumerate() {
                let line = format!(
                    "{:<field_w$} = {}  ({})",
                    created.field,
                    value_repr(&created.value),
                    created_detail(created)
                );
                if i == 0 {
                    new_kv(out, "fields", &line)?;
                } else {
                    writeln!(out, "           {line}")?;
                }
            }
        }

        new_kv(out, "body", &format!("{} bytes", report.body_bytes))?;

        if report.warnings.is_empty() {
            new_kv(out, "warnings", "none")?;
        } else {
            for (i, w) in report.warnings.iter().enumerate() {
                if i == 0 {
                    new_kv(out, "warnings", &warning_short(w))?;
                } else {
                    writeln!(out, "           {}", warning_short(w))?;
                }
            }
        }

        if report.applied {
            writeln!(out, "trace: {}", report.trace_id)?;
        } else {
            writeln!(out)?;
            writeln!(out, "Apply with --yes")?;
        }
        Ok(EXIT_OK)
    })();
    render_outcome(result, conv.writer())
}

// ── edit (body-edit report) ──────────────────────────────────────────────────

fn render_edit<O: Write, E: Write>(
    view: EditMutationView,
    global: &GlobalArgs,
    presenter: &mut Presenter<O, E>,
) -> i32 {
    let format = view.spec.resolve(view.explicit, is_stdout_tty());
    let report = &view.report;
    let (out, err) = presenter.streams();
    let mut conv = Conversation::new(err);

    // A refusal is format-INDEPENDENT for edit (the donor's pre-existing
    // asymmetry, `edit::route::emit`): `error: <message>` on stderr, exit 2, for
    // records AND json alike — unlike set/new, which emit a structured JSON
    // refusal object. `error.message` is the same `Display` prose the donor's
    // direct arm interpolated, so the two are byte-identical.
    if report.outcome == MutationOutcome::Refused {
        let msg = report
            .error
            .as_ref()
            .map(|e| e.message.clone())
            .unwrap_or_else(|| "edit refused".to_string());
        let result: io::Result<i32> = (|| {
            conv.line(&format!("error: {msg}"))?;
            Ok(EXIT_USAGE)
        })();
        return render_outcome(result, conv.writer());
    }

    // JSON: the compact whole-report serialization is the contract (donor
    // `render_json` — `serde_json::to_writer` + one trailing newline).
    if format == Format::Json {
        let result: io::Result<i32> = (|| {
            writeln!(out, "{}", serde_json::to_string(report)?)?;
            Ok(EXIT_OK)
        })();
        return render_outcome(result, conv.writer());
    }

    // Records. Unstyled, like the donor `edit::report::render_records` (it never
    // resolved a palette), so the piped / parity bytes are exact.
    let _ = global;
    let result: io::Result<i32> = (|| {
        let verb = if report.applied {
            "edit"
        } else {
            "dry-run: edit"
        };
        writeln!(out, "{verb} {}", report.target)?;
        for change in &report.edits {
            match change.occurrences {
                Some(n) => writeln!(out, "  {} ({}, {n}×)", change.op, change.anchor)?,
                None => writeln!(out, "  {} ({})", change.op, change.anchor)?,
            }
        }
        if report.body_changed {
            let old = report.body_bytes_old.unwrap_or(0);
            let new = report.body_bytes_new.unwrap_or(0);
            writeln!(out, "  body: {old} → {new} bytes")?;
        }
        // The applied path prints a `trace:` footer after the records block
        // (records only; JSON carries `trace_id` as a field). A forecast prints
        // the blank line + `Apply with --yes` hint instead.
        if report.applied {
            writeln!(out, "trace: {}", report.trace_id)?;
        } else {
            writeln!(out)?;
            writeln!(out, "Apply with --yes")?;
        }
        Ok(EXIT_OK)
    })();
    render_outcome(result, conv.writer())
}

// ── move / delete / rewrite-wikilink (the cascade verbs) ─────────────────────────
//
// These render the shared `ApplyReport` the donor emits (byte-faithful to
// `retired/src/{move,delete,rewrite_wikilink}/route.rs`). `--format json` is the
// report's PRETTY serialization (with a trailing newline); records is the donor's
// human summary. A refused report renders the coded `{code,message,path?}`
// envelope (json, pretty) or `error: <msg>` (records) and exits 2. Cascade
// failures (real FS errors) surface on stderr before the summary.

/// The exit code an `ApplyReport` implies, via its own outcome→exit mapping
/// (applied/dry-run → 0, partial-failure → 1, refused → 2).
fn apply_report_exit(report: &ApplyReport) -> i32 {
    report.exit_code()
}

/// Render a refused cascade `ApplyReport` (the donor `emit_refusal`): the pretty
/// coded error envelope on stdout for `--format json`, else `error: <message>` on
/// stderr. Exit 2.
fn render_apply_refusal(
    report: &ApplyReport,
    json: bool,
    out: &mut dyn Write,
    conv: &mut Conversation,
) -> i32 {
    let error = report
        .operations
        .iter()
        .find_map(|o| o.error.as_ref())
        .or_else(|| report.preconditions.iter().find_map(|p| p.error.as_ref()));
    if json {
        let result: io::Result<i32> = (|| {
            match error {
                Some(e) => writeln!(out, "{}", serde_json::to_string_pretty(e)?)?,
                None => writeln!(out, "{{}}")?,
            }
            Ok(EXIT_USAGE)
        })();
        render_outcome(result, conv.writer())
    } else {
        let msg = error
            .map(|e| e.message.clone())
            .unwrap_or_else(|| "refused".to_string());
        let result: io::Result<i32> = (|| {
            conv.line(&format!("error: {msg}"))?;
            Ok(EXIT_USAGE)
        })();
        render_outcome(result, conv.writer())
    }
}

/// Emit the cascade-failure warnings (real FS errors that left backlinks
/// dangling) on stderr — the donor `emit_cascade_failure_warnings`. A no-op on
/// the common (failure-free) path.
fn emit_cascade_failure_warnings(report: &ApplyReport, w: &mut dyn Write) {
    for op in &report.operations {
        let Some(cascade) = op.cascade.as_ref() else {
            continue;
        };
        if cascade.failed == 0 {
            continue;
        }
        let _ = writeln!(
            w,
            "warning: {} backlink{} could not be rewritten after retries and now dangle{}:",
            cascade.failed,
            if cascade.failed == 1 { "" } else { "s" },
            if cascade.failed == 1 { "s" } else { "" },
        );
        for f in &cascade.failures {
            let _ = match &f.detail {
                Some(d) => writeln!(
                    w,
                    "  {}: {} → {} ({}: {})",
                    f.file, f.from, f.to, f.reason, d
                ),
                None => writeln!(w, "  {}: {} → {} ({})", f.file, f.from, f.to, f.reason),
            };
        }
        let _ = writeln!(
            w,
            "  fix manually, or run `norn validate` to list dangling links."
        );
    }
}

fn render_move<O: Write, E: Write>(view: MoveMutationView, presenter: &mut Presenter<O, E>) -> i32 {
    let report = &view.report;
    let (out, err) = presenter.streams();
    let mut conv = Conversation::new(err);

    if report.outcome == ApplyOutcome::Refused {
        return render_apply_refusal(report, view.json, out, &mut conv);
    }

    emit_cascade_failure_warnings(report, conv.writer());
    let exit = apply_report_exit(report);

    if view.json {
        let result: io::Result<i32> = (|| {
            writeln!(out, "{}", serde_json::to_string_pretty(report)?)?;
            Ok(exit)
        })();
        return render_outcome(result, conv.writer());
    }

    let ascii = glyphs::use_ascii();
    let dry_run = report.dry_run;
    // A folder move's expanded ops carry `from` provenance (the parent
    // `move_folder` op index); a single-file move's `move_document` op does not.
    let is_folder = report.operations.iter().any(|o| o.from.is_some());
    let result: io::Result<i32> = (|| {
        if is_folder {
            render_folder_apply_tty(out, report, dry_run)?;
        } else {
            let (link_total, link_files) = report
                .operations
                .iter()
                .find(|o| o.kind == "move_document")
                .and_then(|o| o.cascade.as_ref())
                .map_or((0, 0), |c| (c.applied, c.files));
            let applied = !dry_run && exit == 0;
            render_move_apply_tty(
                out, &view.src, &view.dst, link_total, link_files, applied, ascii,
            )?;
        }
        if !dry_run {
            writeln!(out, "trace: {}", report.trace_id)?;
        }
        Ok(exit)
    })();
    render_outcome(result, conv.writer())
}

/// Single-file move summary (donor `r#move::render_move_apply_tty`).
fn render_move_apply_tty(
    out: &mut dyn Write,
    src: &str,
    dst: &str,
    link_total: usize,
    link_files: usize,
    applied: bool,
    ascii: bool,
) -> io::Result<()> {
    let arrow = glyphs::render(Glyph::Arrow, ascii);
    if applied {
        writeln!(
            out,
            "{} moved {src} {arrow} {dst}",
            glyphs::render(Glyph::Pass, ascii)
        )?;
        if link_total > 0 {
            writeln!(
                out,
                "{} rewrote {} backlink{} across {} file{}",
                glyphs::render(Glyph::Pass, ascii),
                link_total,
                plural(link_total),
                link_files,
                plural(link_files),
            )?;
        }
    } else {
        writeln!(out, "norn move {src} {arrow} {dst}")?;
        if link_total > 0 {
            writeln!(
                out,
                "  {} backlink{} to rewrite across {} file{}",
                link_total,
                plural(link_total),
                link_files,
                plural(link_files),
            )?;
        } else {
            writeln!(out, "  no backlinks to rewrite")?;
        }
    }
    Ok(())
}

/// Folder move summary (donor `r#move::render_folder_apply_tty`).
fn render_folder_apply_tty(
    out: &mut dyn Write,
    report: &ApplyReport,
    dry_run: bool,
) -> io::Result<()> {
    let status_label = if dry_run { "dry-run" } else { "applied" };
    writeln!(out, "move-folder {status_label}")?;
    writeln!(
        out,
        "  applied: {}  skipped: {}  failed: {}",
        report.applied, report.skipped, report.failed
    )?;
    for op in &report.operations {
        let status = format!("{:?}", op.status).to_lowercase();
        writeln!(out, "  [{status}] {}", op.summary)?;
    }
    Ok(())
}

fn render_delete<O: Write, E: Write>(
    view: DeleteMutationView,
    presenter: &mut Presenter<O, E>,
) -> i32 {
    let report = &view.report;
    let (out, err) = presenter.streams();
    let mut conv = Conversation::new(err);

    if report.outcome == ApplyOutcome::Refused {
        return render_apply_refusal(report, view.json, out, &mut conv);
    }

    emit_cascade_failure_warnings(report, conv.writer());
    let exit = apply_report_exit(report);

    if view.json {
        let result: io::Result<i32> = (|| {
            writeln!(out, "{}", serde_json::to_string_pretty(report)?)?;
            Ok(exit)
        })();
        return render_outcome(result, conv.writer());
    }

    let ascii = glyphs::use_ascii();
    let dry_run = report.dry_run;
    let result: io::Result<i32> = (|| {
        render_delete_records(out, report, &view.doc, dry_run, exit, ascii)?;
        Ok(exit)
    })();
    render_outcome(result, conv.writer())
}

/// Records-format delete output (donor `delete::render_delete_records`).
fn render_delete_records(
    out: &mut dyn Write,
    report: &ApplyReport,
    doc: &str,
    dry_run: bool,
    exit: i32,
    ascii: bool,
) -> io::Result<()> {
    let delete_op = report
        .operations
        .iter()
        .find(|o| o.kind == "delete_document");
    let (incoming_total, incoming_files, redirect_to): (usize, &[String], Option<&str>) = delete_op
        .and_then(|o| o.link_impact.as_ref())
        .map_or((0, &[][..], None), |li| {
            (
                li.incoming_total,
                li.incoming_files.as_slice(),
                li.redirect_to.as_deref(),
            )
        });
    let rewrite_total = delete_op
        .and_then(|o| o.cascade.as_ref())
        .map_or(0, |c| c.applied);
    let applied = !dry_run && exit == 0;
    render_delete_apply_tty(
        out,
        doc,
        incoming_total,
        incoming_files,
        redirect_to,
        rewrite_total,
        applied,
        ascii,
    )?;
    if !dry_run {
        writeln!(out, "trace: {}", report.trace_id)?;
    }
    Ok(())
}

/// The delete summary (donor `delete::render_delete_apply_tty`).
#[allow(clippy::too_many_arguments)]
fn render_delete_apply_tty(
    out: &mut dyn Write,
    doc: &str,
    incoming_total: usize,
    incoming_files: &[String],
    rewrite_to: Option<&str>,
    rewrite_total: usize,
    applied: bool,
    ascii: bool,
) -> io::Result<()> {
    let pass = glyphs::render(Glyph::Pass, ascii);
    let warn = glyphs::render(Glyph::Warn, ascii);
    if applied {
        match rewrite_to {
            Some(alt) => {
                writeln!(
                    out,
                    "{pass} deleted {doc} (incoming links redirected to {alt})"
                )?;
                writeln!(
                    out,
                    "{pass} rewrote {} {} across {} {}",
                    rewrite_total,
                    noun(rewrite_total, "backlink", "backlinks"),
                    incoming_files.len(),
                    noun(incoming_files.len(), "file", "files"),
                )?;
            }
            None => {
                writeln!(out, "{pass} deleted {doc}")?;
                if incoming_total > 0 {
                    writeln!(
                        out,
                        "{warn} {} {} now broken (surface via norn validate)",
                        incoming_total,
                        noun(incoming_total, "link", "links"),
                    )?;
                }
            }
        }
    } else {
        match rewrite_to {
            Some(alt) => {
                writeln!(
                    out,
                    "norn delete {doc} {} redirects {} incoming {} to {alt}",
                    glyphs::render(Glyph::Arrow, ascii),
                    incoming_total,
                    noun(incoming_total, "link", "links"),
                )?;
                writeln!(
                    out,
                    "  {} {} to rewrite across {} {}",
                    rewrite_total,
                    noun(rewrite_total, "backlink", "backlinks"),
                    incoming_files.len(),
                    noun(incoming_files.len(), "file", "files"),
                )?;
            }
            None => {
                writeln!(out, "norn delete {doc}")?;
                if incoming_total > 0 {
                    writeln!(
                        out,
                        "  {warn} {} incoming {} will break across {} {}:",
                        incoming_total,
                        noun(incoming_total, "link", "links"),
                        incoming_files.len(),
                        noun(incoming_files.len(), "file", "files"),
                    )?;
                    for file in incoming_files {
                        writeln!(out, "      {file}")?;
                    }
                    writeln!(
                        out,
                        "  (broken links will surface as link-target-missing findings in `norn validate`)"
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn render_rewrite_wikilink<O: Write, E: Write>(
    view: RewriteWikilinkView,
    presenter: &mut Presenter<O, E>,
) -> i32 {
    let report = &view.report;
    let (out, err) = presenter.streams();
    let mut conv = Conversation::new(err);

    if report.outcome == ApplyOutcome::Refused {
        // `--out` is a write-to-file projection of the report; a refusal never
        // reaches it (the donor refuses before rendering), so the envelope path
        // is unconditional here.
        return render_apply_refusal(report, view.json, out, &mut conv);
    }

    emit_cascade_failure_warnings(report, conv.writer());
    let exit = apply_report_exit(report);

    // `--out`: write the (always-JSON, pretty) report to the file, silence stdout.
    if let Some(out_path) = &view.out {
        let result: io::Result<i32> = (|| {
            let json = serde_json::to_string_pretty(report)?;
            std::fs::write(out_path, format!("{json}\n"))?;
            Ok(exit)
        })();
        return render_outcome(result, conv.writer());
    }

    if view.json {
        let result: io::Result<i32> = (|| {
            writeln!(out, "{}", serde_json::to_string_pretty(report)?)?;
            Ok(exit)
        })();
        return render_outcome(result, conv.writer());
    }

    let result: io::Result<i32> = (|| {
        render_rewrite_records(out, report, &view.old, &view.new)?;
        if !report.dry_run {
            writeln!(out, "trace: {}", report.trace_id)?;
        }
        Ok(exit)
    })();
    render_outcome(result, conv.writer())
}

/// Records-format rewrite-wikilink output (donor `rewrite_wikilink::render_records`).
fn render_rewrite_records(
    out: &mut dyn Write,
    report: &ApplyReport,
    old: &str,
    new: &str,
) -> io::Result<()> {
    let body_count = report
        .operations
        .iter()
        .filter(|o| o.kind == "rewrite_link")
        .count();
    let fm_count = report
        .operations
        .iter()
        .filter(|o| o.kind == "set_frontmatter")
        .count();
    let total = report.operations.len();
    let status = if report.dry_run {
        "would rewrite"
    } else {
        "rewrote"
    };
    writeln!(
        out,
        "{status} [[{old}]] → [[{new}]] in {total} ops ({body_count} body + {fm_count} frontmatter)"
    )?;
    if !report.warnings.is_empty() {
        writeln!(out, "warnings:")?;
        for w in &report.warnings {
            writeln!(out, "  {}: {}", w.code, w.message)?;
        }
    }
    Ok(())
}

/// `""` for a count of 1, `"s"` otherwise — the donor pluralization.
fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Singular/plural noun selector for the delete summary.
fn noun(n: usize, singular: &'static str, plural: &'static str) -> &'static str {
    if n == 1 {
        singular
    } else {
        plural
    }
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
            dynamic_fields: Vec::new(),
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
            sort_field: None,
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
            by: vec![],
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

    // ── NRN-374: unknown `--sort`/`--by` warnings (--col precedent) ──────

    fn find_doc_with(fm: Value) -> FindDoc {
        FindDoc {
            frontmatter: Some(fm),
            ..one_find_doc()
        }
    }

    #[test]
    fn warn_unknown_sort_find_warns_once_when_absent_from_every_match() {
        let report = FindReport {
            documents: vec![find_doc_with(json!({"title": "A"}))],
            total: 1,
            returned: 1,
            starts_at: 1,
            truncated: false,
        };
        let mut err = Vec::new();
        warn_unknown_sort_find(&report, Some("priorty"), &mut err).unwrap();
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "warning: --sort field `priorty` not present in any matching document\n"
        );
    }

    #[test]
    fn warn_unknown_sort_find_silent_for_a_present_field() {
        let report = FindReport {
            documents: vec![find_doc_with(json!({"title": "A"}))],
            total: 1,
            returned: 1,
            starts_at: 1,
            truncated: false,
        };
        let mut err = Vec::new();
        warn_unknown_sort_find(&report, Some("title"), &mut err).unwrap();
        assert!(err.is_empty(), "known field must not warn: {err:?}");
    }

    #[test]
    fn warn_unknown_sort_find_exempts_structural_path_and_stem() {
        let report = FindReport {
            documents: vec![find_doc_with(json!({}))],
            total: 1,
            returned: 1,
            starts_at: 1,
            truncated: false,
        };
        for structural in ["path", "stem"] {
            let mut err = Vec::new();
            warn_unknown_sort_find(&report, Some(structural), &mut err).unwrap();
            assert!(
                err.is_empty(),
                "{structural} is a virtual sort key, never a frontmatter field: {err:?}"
            );
        }
    }

    #[test]
    fn warn_unknown_sort_find_skips_a_zero_match_result() {
        let report = FindReport {
            documents: vec![],
            total: 0,
            returned: 0,
            starts_at: 1,
            truncated: false,
        };
        let mut err = Vec::new();
        warn_unknown_sort_find(&report, Some("anything"), &mut err).unwrap();
        assert!(
            err.is_empty(),
            "a zero-match result must not warn on every field: {err:?}"
        );
    }

    // The truncated-page edge (a real find bug an adversarial review caught):
    // a sparse field present only beyond the returned page must never warn —
    // and, pinned alongside it, an UNTRUNCATED result with the same absent
    // field must still warn (the pre-fix tests all happened to use
    // total == returned, which left this pair unpinned).

    #[test]
    fn warn_unknown_sort_find_skips_a_truncated_page() {
        // 12 total matches, --limit narrows the page to 1 returned document
        // that happens to lack `priorty` — the field could easily live on one
        // of the other 11 matches beyond the page boundary, so this must NOT
        // warn.
        let report = FindReport {
            documents: vec![find_doc_with(json!({"title": "A"}))],
            total: 12,
            returned: 1,
            starts_at: 1,
            truncated: true,
        };
        let mut err = Vec::new();
        warn_unknown_sort_find(&report, Some("priorty"), &mut err).unwrap();
        assert!(
            err.is_empty(),
            "a truncated page must not warn on a field absent only from the page: {err:?}"
        );
    }

    #[test]
    fn warn_unknown_sort_find_warns_when_untruncated_and_absent() {
        // Same shape as the truncated case above but the FULL result set (no
        // --limit narrowing) — every match was inspected, so the field truly
        // is absent and the warning must still fire.
        let report = FindReport {
            documents: vec![find_doc_with(json!({"title": "A"}))],
            total: 1,
            returned: 1,
            starts_at: 1,
            truncated: false,
        };
        let mut err = Vec::new();
        warn_unknown_sort_find(&report, Some("priorty"), &mut err).unwrap();
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "warning: --sort field `priorty` not present in any matching document\n"
        );
    }

    #[test]
    fn warn_unknown_sort_find_is_a_no_op_without_sort() {
        let report = FindReport {
            documents: vec![find_doc_with(json!({}))],
            total: 1,
            returned: 1,
            starts_at: 1,
            truncated: false,
        };
        let mut err = Vec::new();
        warn_unknown_sort_find(&report, None, &mut err).unwrap();
        assert!(err.is_empty());
    }

    #[test]
    fn render_find_with_unknown_sort_field_still_exits_ok_and_warns() {
        let mut view = find_view();
        view.report.documents = vec![find_doc_with(json!({"title": "A"}))];
        view.sort_field = Some("priorty".to_string());
        let global = global_args();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut presenter = Presenter::new(&mut out, &mut err);
            render_find(view, &global, &mut presenter)
        };
        assert_eq!(
            code, EXIT_OK,
            "the query still runs; a warning never flips the exit code"
        );
        let stderr = String::from_utf8(err).unwrap();
        assert_eq!(
            stderr.matches("--sort field `priorty`").count(),
            1,
            "warns exactly once: {stderr:?}"
        );
    }

    #[test]
    fn warn_unknown_sort_get_warns_with_the_get_warn_prefix() {
        let report = GetReport {
            records: vec![get_record("a.md", json!({"title": "A"}))],
            notes: vec![],
            markdown_content: None,
        };
        let mut err = Vec::new();
        warn_unknown_sort_get(&report, Some("priorty"), &mut err).unwrap();
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "warn: --sort field 'priorty' not present in document\n"
        );
    }

    #[test]
    fn warn_unknown_sort_get_silent_for_a_present_field() {
        let report = GetReport {
            records: vec![get_record("a.md", json!({"title": "A"}))],
            notes: vec![],
            markdown_content: None,
        };
        let mut err = Vec::new();
        warn_unknown_sort_get(&report, Some("title"), &mut err).unwrap();
        assert!(err.is_empty());
    }

    #[test]
    fn warn_unknown_by_count_warns_when_grouped_is_entirely_missing() {
        let mut groups = BTreeMap::new();
        groups.insert(MISSING_BUCKET.to_string(), 3usize);
        let report = CountReport::Grouped {
            by: "priorty".to_string(),
            total: 3,
            groups,
        };
        let mut err = Vec::new();
        warn_unknown_by_count(&report, &mut err).unwrap();
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "warning: --by field `priorty` not present in any matching document\n"
        );
    }

    #[test]
    fn warn_unknown_by_count_silent_when_some_docs_carry_the_field() {
        let mut groups = BTreeMap::new();
        groups.insert("active".to_string(), 2usize);
        groups.insert(MISSING_BUCKET.to_string(), 1usize);
        let report = CountReport::Grouped {
            by: "status".to_string(),
            total: 3,
            groups,
        };
        let mut err = Vec::new();
        warn_unknown_by_count(&report, &mut err).unwrap();
        assert!(
            err.is_empty(),
            "a partially-missing field is known, not unknown: {err:?}"
        );
    }

    #[test]
    fn warn_unknown_by_count_silent_for_the_bare_total_variant() {
        let report = CountReport::Total { total: 3 };
        let mut err = Vec::new();
        warn_unknown_by_count(&report, &mut err).unwrap();
        assert!(err.is_empty(), "no --by field was requested: {err:?}");
    }

    #[test]
    fn warn_unknown_by_count_checks_only_the_outermost_grouped_multi_field() {
        let mut inner = BTreeMap::new();
        inner.insert(MISSING_BUCKET.to_string(), GroupNode::Leaf(3));
        let mut outer = BTreeMap::new();
        outer.insert(MISSING_BUCKET.to_string(), GroupNode::Branch(inner));
        let report = CountReport::GroupedMulti {
            by: vec!["priorty".to_string(), "status".to_string()],
            total: 3,
            groups: outer,
        };
        let mut err = Vec::new();
        warn_unknown_by_count(&report, &mut err).unwrap();
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "warning: --by field `priorty` not present in any matching document\n",
            "only the first --by field is checked (a documented scoped simplification)"
        );
    }

    #[test]
    fn render_count_with_unknown_by_field_still_exits_ok_and_warns() {
        let mut groups = BTreeMap::new();
        groups.insert(MISSING_BUCKET.to_string(), 3usize);
        let view = CountView {
            report: CountReport::Grouped {
                by: "priorty".to_string(),
                total: 3,
                groups,
            },
            explicit: Some(Format::Json),
            spec: FormatSpec {
                tty: Format::Records,
                piped: Format::Records,
            },
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut presenter = Presenter::new(&mut out, &mut err);
            render_count(view, &mut presenter)
        };
        assert_eq!(code, EXIT_OK);
        assert!(!out.is_empty(), "the count still renders its normal output");
        assert!(String::from_utf8(err)
            .unwrap()
            .contains("--by field `priorty`"));
    }

    #[test]
    fn warn_unknown_by_describe_warns_when_the_field_was_dropped() {
        let report = describe_sample(); // data.fields carries only "type"
        let mut err = Vec::new();
        warn_unknown_by_describe(&report, &["priorty".to_string()], &mut err).unwrap();
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "warning: --by field `priorty` not present in any matching document\n"
        );
    }

    #[test]
    fn warn_unknown_by_describe_silent_when_the_field_is_present() {
        let report = describe_sample();
        let mut err = Vec::new();
        warn_unknown_by_describe(&report, &["type".to_string()], &mut err).unwrap();
        assert!(err.is_empty());
    }

    #[test]
    fn warn_unknown_by_describe_skips_when_data_mode_is_off() {
        let mut report = describe_sample();
        report.data = None;
        let mut err = Vec::new();
        warn_unknown_by_describe(&report, &["priorty".to_string()], &mut err).unwrap();
        assert!(err.is_empty(), "no --data/--by was requested: {err:?}");
    }

    #[test]
    fn warn_unknown_by_describe_skips_a_zero_match_result() {
        let mut report = describe_sample();
        report.data.as_mut().unwrap().total = 0;
        let mut err = Vec::new();
        warn_unknown_by_describe(&report, &["priorty".to_string()], &mut err).unwrap();
        assert!(
            err.is_empty(),
            "a zero-match result must not warn on every field: {err:?}"
        );
    }

    #[test]
    fn warn_unknown_by_describe_ignores_whitespace_only_entries() {
        let report = describe_sample();
        let mut err = Vec::new();
        warn_unknown_by_describe(&report, &[" ".to_string(), "".to_string()], &mut err).unwrap();
        assert!(err.is_empty());
    }

    #[test]
    fn render_describe_with_unknown_by_field_still_exits_ok_and_warns() {
        let mut view = describe_view();
        view.by = vec!["priorty".to_string()];
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut presenter = Presenter::new(&mut out, &mut err);
            render_describe(view, &mut presenter)
        };
        assert_eq!(code, EXIT_OK);
        assert!(!out.is_empty());
        assert!(String::from_utf8(err)
            .unwrap()
            .contains("--by field `priorty`"));
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

    // ── validate records renderers (ported from the donor `validate::render`
    // suite, adapted to the Value-fed `render_validate_full` / `_summary` seam;
    // input order is fixed per test so the renderers are exercised
    // deterministically despite the engine's order nondeterminism) ────────────

    /// A minimal finding value — the renderers read only these four fields.
    fn vf(code: &str, severity: &str, path: &str, message: &str) -> Value {
        json!({
            "code": code,
            "severity": severity,
            "path": path,
            "message": message,
        })
    }

    /// Three warning findings across three docs: two share a code (grouping),
    /// one is a second code — the donor `sample_findings` shape.
    fn sample_validate_findings() -> Vec<Value> {
        vec![
            vf(
                "frontmatter-required-field-missing",
                "warning",
                "notes/welcome.md",
                "required frontmatter field is missing: kind",
            ),
            vf(
                "frontmatter-required-field-missing",
                "warning",
                "notes/draft.md",
                "required frontmatter field is missing: kind",
            ),
            vf(
                "document-misrouted",
                "warning",
                "inbox/2026-05-12.md",
                "document path is outside allowed rule locations",
            ),
        ]
    }

    fn full(findings: &[Value], palette: &Palette, total_docs: usize) -> String {
        let mut buf = Vec::new();
        render_validate_full(&mut buf, palette, findings, 12, total_docs).unwrap();
        String::from_utf8(buf).unwrap()
    }

    fn summary(findings: &[Value], palette: &Palette, total_docs: usize) -> String {
        let mut buf = Vec::new();
        render_validate_summary(&mut buf, palette, findings, 12, total_docs, 80).unwrap();
        String::from_utf8(buf).unwrap()
    }

    // ── summary view ─────────────────────────────────────────────────────────

    #[test]
    fn validate_summary_emits_status_headline() {
        let s = summary(&sample_validate_findings(), &Palette::off(), 780);
        let first = s.lines().next().unwrap();
        assert!(
            first.starts_with("running 12 rules across 780 documents"),
            "headline: {first:?}"
        );
        assert!(first.ends_with('…'), "headline ellipsis: {first:?}");
    }

    #[test]
    fn validate_summary_emits_severity_tally() {
        let s = summary(&sample_validate_findings(), &Palette::off(), 780);
        // 3 unique docs with findings → 780 − 3 = 777 pass; all 3 are warnings.
        assert!(s.contains("777 documents pass"), "expected pass row: {s:?}");
        assert!(s.contains("3 warnings"), "expected warning row: {s:?}");
    }

    #[test]
    fn validate_summary_emits_by_code_tally_group() {
        let s = summary(&sample_validate_findings(), &Palette::off(), 780);
        assert!(s.contains("  by code"));
        assert!(s.contains("frontmatter-required-field-missing"));
        assert!(s.contains("document-misrouted"));
    }

    #[test]
    fn validate_summary_no_findings_emits_clean_tally_and_no_by_code() {
        let s = summary(&[], &Palette::off(), 780);
        assert!(s.contains("780 documents pass"));
        assert!(!s.contains("by code"));
    }

    #[test]
    fn validate_summary_color_off_no_ansi_color_on_ansi() {
        assert!(!summary(&sample_validate_findings(), &Palette::off(), 780).contains('\u{1b}'));
        assert!(summary(&sample_validate_findings(), &Palette::on(), 780).contains('\u{1b}'));
    }

    // ── full view ────────────────────────────────────────────────────────────

    #[test]
    fn validate_full_emits_status_headline() {
        let s = full(&sample_validate_findings(), &Palette::off(), 780);
        assert!(s
            .lines()
            .next()
            .unwrap()
            .starts_with("running 12 rules across 780 documents"));
    }

    #[test]
    fn validate_full_groups_by_code_with_both_headers() {
        let s = full(&sample_validate_findings(), &Palette::off(), 780);
        assert!(
            s.contains("frontmatter-required-field-missing") && s.contains("document-misrouted"),
            "expected both code headers: {s:?}"
        );
    }

    #[test]
    fn validate_full_path_at_2_indent_message_at_4_indent() {
        let s = full(&sample_validate_findings(), &Palette::off(), 780);
        assert!(
            s.contains("\n  notes/welcome.md"),
            "expected 2-indent path: {s:?}"
        );
        assert!(
            s.contains("\n    required frontmatter"),
            "expected 4-indent message: {s:?}"
        );
    }

    #[test]
    fn validate_full_emits_fix_hint_for_known_codes() {
        let s = full(&sample_validate_findings(), &Palette::off(), 780);
        assert!(
            s.contains("    fix: add the field"),
            "expected fix hint for required-field-missing: {s:?}"
        );
        assert!(
            s.contains("    fix: move the document"),
            "expected fix hint for document-misrouted: {s:?}"
        );
    }

    #[test]
    fn validate_full_omits_fix_when_code_unknown() {
        let s = full(
            &[vf("not-a-real-code", "warning", "x.md", "fake")],
            &Palette::off(),
            780,
        );
        assert!(
            !s.contains("    fix:"),
            "unknown code has no fix hint: {s:?}"
        );
    }

    #[test]
    fn validate_full_footer_shows_pass_count_and_findings_shown() {
        let s = full(&sample_validate_findings(), &Palette::off(), 780);
        let footer = s.lines().last().unwrap();
        assert!(
            footer.contains("777 documents pass"),
            "footer pass count: {footer:?}"
        );
        assert!(
            footer.contains("3 findings shown"),
            "footer findings: {footer:?}"
        );
    }

    #[test]
    fn validate_full_no_findings_collapses_to_clean_tally() {
        let s = full(&[], &Palette::off(), 780);
        assert!(s.contains("780 documents pass"));
        assert!(!s.contains("fix:"));
    }

    #[test]
    fn validate_full_severity_selects_glyph_color_per_group() {
        // Under Palette::on(), a warning group header carries amber (ansi 178)
        // and an error group header carries rune (ansi 167) — locale-independent,
        // unlike the ✓/⚠/✗ glyph choice.
        let findings = vec![
            vf(
                "link-target-missing",
                "warning",
                "a.md",
                "link target not found: x",
            ),
            vf(
                "frontmatter-parse-failed",
                "error",
                "b.md",
                "frontmatter failed to parse",
            ),
        ];
        let s = full(&findings, &Palette::on(), 780);
        assert!(
            s.contains("\x1b[38;5;178m"),
            "expected amber on the warning group header: {s:?}"
        );
        assert!(
            s.contains("\x1b[38;5;167m"),
            "expected rune on the error group header: {s:?}"
        );
    }

    // ── mutation reporting (review fixes F1/F3/F5) ─────────────────────────────

    use norn_wire::{FrontmatterCreated, MutationWarning, NewReport};

    fn created(field: &str, value: Value, source: &str, rule: Option<&str>) -> FrontmatterCreated {
        FrontmatterCreated {
            field: field.into(),
            value,
            source: source.into(),
            rule: rule.map(str::to_string),
        }
    }

    #[test]
    fn f1_created_detail_uses_the_core_vocabulary_verbatim() {
        // No remap layer: the source label is whatever the core set, plus the
        // crediting rule when a default carried one.
        assert_eq!(
            created_detail(&created(
                "type",
                json!("note"),
                "schema-default",
                Some("typed-note")
            )),
            "schema-default, typed-note"
        );
        assert_eq!(
            created_detail(&created("title", json!("X"), "operator-flag", None)),
            "operator-flag"
        );
        assert_eq!(
            created_detail(&created("tags", json!([1]), "operator-flag-json", None)),
            "operator-flag-json"
        );
    }

    #[test]
    fn f3_new_records_pads_field_names_to_the_widest() {
        let report = NewReport {
            schema_version: 2,
            trace_id: String::new(),
            operation: "new".into(),
            path: Some("a.md".into()),
            applied: false,
            outcome: MutationOutcome::Applied,
            frontmatter_created: vec![
                created("kind", json!("note"), "schema-default", Some("r")),
                created("verylongfield", json!("v"), "operator-flag", None),
            ],
            body_bytes: 0,
            warnings: vec![],
            predicted_path: None,
            error: None,
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut presenter = Presenter::new(&mut out, &mut err);
            render_new(
                NewMutationView {
                    report,
                    explicit: Some(Format::Records),
                    spec: FormatSpec {
                        tty: Format::Records,
                        piped: Format::Records,
                    },
                },
                &global_args(),
                &mut presenter,
            );
        }
        let s = String::from_utf8(out).unwrap();
        // Both field cells are padded to the widest name (13 chars), so the `=`
        // columns align.
        assert!(s.contains("kind          = note"), "{s}");
        assert!(s.contains("verylongfield = v"), "{s}");
    }

    #[test]
    fn f5_warning_short_rebuilds_the_donor_records_labels_per_code() {
        let uf = MutationWarning {
            code: "unknown-field".into(),
            field: Some("status".into()),
            message: "field 'status' not declared in schema".into(),
        };
        assert_eq!(warning_short(&uf), "unknown field: status");

        let ti = MutationWarning {
            code: "title-ignored".into(),
            field: None,
            message: "--title 'X' has no effect with an explicit path".into(),
        };
        assert_eq!(
            warning_short(&ti),
            "title-ignored: --title 'X' has no effect with an explicit path"
        );

        let fb = MutationWarning {
            code: "force-bypass".into(),
            field: Some("status".into()),
            message: "--force bypassed type validation for 'status'".into(),
        };
        assert_eq!(warning_short(&fb), "--force bypass: status");
    }
}
