//! `get` (NRN-409): the projection ladder shared with `find`, plus the
//! `--format markdown` byte-faithful source passthrough.

use std::io::{self, Write};

use norn_wire::{GetRecord, GetReport};
use serde_json::Value;

use crate::display::conversation::Conversation;
use crate::display::emit::render_outcome;
use crate::display::format::Format;
use crate::display::output::GetView;
use crate::display::sink::{Field, Sink};
use crate::display::{EXIT_OK, EXIT_OPERATIONAL};
use crate::output::palette::Palette;
use crate::output::projection::{
    project_json, project_pairs, split_cols, unknown_facet_message, warn_col_ignored,
    warn_section_ignored, DefaultCols, DocView, KNOWN_FACETS,
};

pub(crate) fn render_get(
    view: GetView,
    format: Format,
    sink: &mut Sink<'_>,
    conv: &mut Conversation<'_>,
) -> i32 {
    if format == Format::Markdown {
        // Byte-faithful source passthrough — never colorized, so no palette.
        return render_get_markdown(&view, sink.writer(), conv);
    }

    let text = match format {
        Format::Json => render_get_json(&view.report, &view.cols),
        Format::Jsonl => render_get_jsonl(&view.report, &view.cols),
        Format::Paths => render_get_paths(&view.report),
        Format::Records => {
            // NRN-362: get records render through the SAME resolved palette find
            // uses — threaded in via the sink. Piped (pipelines) it is
            // off → bytes unchanged.
            render_get_records(&view.report, sink.palette(), sink.width(), &view.cols)
        }
        Format::Markdown => unreachable!("markdown handled above"),
    };
    let result: io::Result<i32> = (|| {
        // Exactly one trailing newline.
        if text.ends_with('\n') {
            write!(sink.writer(), "{text}")?;
        } else {
            writeln!(sink.writer(), "{text}")?;
        }

        let paths_inert = (format == Format::Paths).then_some("paths");
        warn_col_ignored(&view.cols, paths_inert, conv)?;
        warn_section_ignored(&view.sections, paths_inert, conv)?;
        warn_unknown_cols_get(&view.cols, &view.report, conv)?;
        warn_unknown_sort_get(&view.report, view.sort_field.as_deref(), conv)?;
        for note in &view.report.notes {
            conv.report_note(note)?;
        }

        Ok(if has_error(&view.report) {
            EXIT_OPERATIONAL
        } else {
            EXIT_OK
        })
    })();

    render_outcome(result, conv.writer())
}

/// `--format markdown`: the exact source bytes. Refuses unless exactly one
/// document resolved; `--col`/`--section` are ignored (warned). The refusal
/// itself is an owner-side `error`-severity [`Note`](norn_wire::Note) (NRN-460,
/// `crates/norn-owner`'s `read_markdown_source`) — every routed surface refuses
/// identically, so this renderer only prints the note and derives its exit code
/// from [`has_error`]; it owns no refusal text of its own.
fn render_get_markdown(view: &GetView, out: &mut dyn Write, conv: &mut Conversation<'_>) -> i32 {
    let result: io::Result<i32> = (|| {
        warn_col_ignored(&view.cols, Some("markdown"), conv)?;
        warn_section_ignored(&view.sections, Some("markdown"), conv)?;
        for note in &view.report.notes {
            conv.report_note(note)?;
        }

        if let Some(content) = &view.report.markdown_content {
            write!(out, "{content}")?;
        }

        Ok(if has_error(&view.report) {
            EXIT_OPERATIONAL
        } else {
            EXIT_OK
        })
    })();

    render_outcome(result, conv.writer())
}

/// The single get "failure" signal: an `error`-severity note drives exit 1. The
/// decision reads the note's typed [`Severity`](norn_wire::Severity), never its
/// message text (ADR 0022).
fn has_error(report: &GetReport) -> bool {
    report.notes.iter().any(|n| n.is_error())
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

fn render_get_records(
    report: &GetReport,
    palette: &Palette,
    width: usize,
    cols: &[String],
) -> String {
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

/// Warn for `--col` tokens that won't resolve, on the closed `warning:` prefix
/// (the same annotation vocabulary `find` and every other verb speaks).
///
/// The bare-field "not present in document" check is guarded to a non-empty
/// record set, matching the sibling `warn_unknown_sort_get` zero-match skip
/// (NRN-44): every field is trivially absent from an empty set, so the warning
/// would be noise about the empty result rather than the field. `get` resolves
/// explicit targets (no `--limit` paging), so there is no truncation guard —
/// only the empty one. The unknown-FACET check stays unconditional.
fn warn_unknown_cols_get(
    cols: &[String],
    report: &GetReport,
    conv: &mut Conversation<'_>,
) -> io::Result<()> {
    let (facets, fields) = split_cols(cols);
    for facet in &facets {
        if !KNOWN_FACETS.contains(&facet.as_str()) {
            conv.warning(&unknown_facet_message(facet))?;
        }
    }
    if report.records.is_empty() {
        return Ok(());
    }
    for field in &fields {
        let present_in_any = report.records.iter().any(|r| {
            r.frontmatter
                .as_ref()
                .and_then(Value::as_object)
                .is_some_and(|obj| obj.contains_key(field))
        });
        if !present_in_any {
            conv.warning(&format!(
                "--col field '{field}' not present in document (bare names select frontmatter fields; use '.{field}' for a structural facet)"
            ))?;
        }
    }
    Ok(())
}

/// `get`'s counterpart to `warn_unknown_sort_find` (NRN-374): same field-absent
/// check over the resolved records, same `path`/`stem` exemption and zero-match
/// skip, on the closed `warning:` prefix.
fn warn_unknown_sort_get(
    report: &GetReport,
    sort_field: Option<&str>,
    conv: &mut Conversation<'_>,
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
        conv.warning(&format!("--sort field '{field}' not present in document"))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::GlobalArgs;
    use crate::display::format::{FormatChoice, FormatSpec};
    use crate::display::Presenter;
    use crate::test_support::{global_args, FailingWriter};
    use norn_wire::Note;
    use serde_json::json;

    /// Drive `render_get` through the same resolution `emit` performs.
    fn drive<O: Write, E: Write>(
        view: GetView,
        global: &GlobalArgs,
        presenter: &mut Presenter<O, E>,
    ) -> i32 {
        let format = view.format.resolve(false);
        let palette = crate::output::palette::resolve(global.color);
        let (out, err) = presenter.streams();
        let mut sink = Sink::new(out, &palette, 80);
        let mut conv = Conversation::new(err);
        render_get(view, format, &mut sink, &mut conv)
    }

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
        let text = render_get_records(&report, &Palette::off(), 80, &[]);
        assert!(text.contains("a.md"), "path header: {text}");
        assert!(text.contains("title"), "frontmatter field: {text}");
        assert!(text.contains("headings"), "headings row: {text}");
    }

    #[test]
    fn get_records_colorize_under_an_enabled_palette() {
        // NRN-362: get honors the resolved palette. Color on → ANSI escapes;
        // off (the piped path) → bytes unchanged.
        let report = GetReport {
            records: vec![get_record("a.md", json!({"title": "A"}))],
            notes: vec![],
            markdown_content: None,
        };
        let colored = render_get_records(&report, &Palette::on(), 80, &[]);
        let plain = render_get_records(&report, &Palette::off(), 80, &[]);
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
            notes: vec![Note::error(
                "target-not-found",
                "'x' did not resolve to any doc",
            )],
            markdown_content: None,
        };
        assert!(has_error(&report));
        let ok = GetReport {
            records: vec![],
            notes: vec![Note::warning("target-ambiguous", "'x' resolved to 2 docs")],
            markdown_content: None,
        };
        assert!(!has_error(&ok));
    }

    fn get_view(explicit: Format, report: GetReport) -> GetView {
        GetView {
            report,
            cols: vec![],
            sections: vec![],
            sort_field: None,
            format: FormatChoice {
                explicit: Some(explicit),
                spec: FormatSpec {
                    tty: Format::Records,
                    piped: Format::Records,
                },
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
            drive(get_view(Format::Paths, report), &global, &mut presenter)
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
            drive(get_view(Format::Paths, report), &global, &mut presenter)
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
            drive(get_view(Format::Markdown, report), &global, &mut presenter)
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
            drive(get_view(Format::Markdown, report), &global, &mut presenter)
        };
        assert_eq!(code, EXIT_OPERATIONAL);
        assert!(String::from_utf8(err).unwrap().starts_with("norn: "));
    }

    #[test]
    fn warn_unknown_sort_get_warns_with_the_get_warn_prefix() {
        let report = GetReport {
            records: vec![get_record("a.md", json!({"title": "A"}))],
            notes: vec![],
            markdown_content: None,
        };
        let mut err = Vec::new();
        let mut conv = Conversation::new(&mut err);
        warn_unknown_sort_get(&report, Some("priorty"), &mut conv).unwrap();
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "warning: --sort field 'priorty' not present in document\n"
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
        let mut conv = Conversation::new(&mut err);
        warn_unknown_sort_get(&report, Some("title"), &mut conv).unwrap();
        assert!(err.is_empty());
    }

    #[test]
    fn warn_unknown_cols_get_warns_for_an_absent_field_on_a_resolved_record() {
        let report = GetReport {
            records: vec![get_record("a.md", json!({"title": "A"}))],
            notes: vec![],
            markdown_content: None,
        };
        let mut err = Vec::new();
        let mut conv = Conversation::new(&mut err);
        warn_unknown_cols_get(&["priorty".to_string()], &report, &mut conv).unwrap();
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "warning: --col field 'priorty' not present in document (bare names select frontmatter fields; use '.priorty' for a structural facet)\n"
        );
    }

    #[test]
    fn warn_unknown_cols_get_skips_a_zero_record_set() {
        // NRN-44: no resolved records means every field is trivially absent —
        // the warning would be noise about the empty result, not the field.
        let report = GetReport {
            records: vec![],
            notes: vec![],
            markdown_content: None,
        };
        let mut err = Vec::new();
        let mut conv = Conversation::new(&mut err);
        warn_unknown_cols_get(&["priorty".to_string()], &report, &mut conv).unwrap();
        assert!(
            err.is_empty(),
            "an empty record set must not warn on a --col field: {err:?}"
        );
    }
}
