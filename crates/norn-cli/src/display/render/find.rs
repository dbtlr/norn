//! `find` (NRN-409): the projection ladder shared with `get`.

use std::io::{self, Write};

use norn_wire::{FindDoc, FindReport};
use serde_json::Value;

use crate::display::conversation::Conversation;
use crate::display::emit::render_outcome;
use crate::display::format::Format;
use crate::display::output::FindView;
use crate::display::sink::{Field, Sink};
use crate::display::EXIT_OK;
use crate::output::projection::{
    project_json, project_pairs, split_cols, unknown_facet_message, warn_col_ignored, DefaultCols,
    DocView, KNOWN_FACETS,
};

use super::shared::truncation_note;

pub(crate) fn render_find(
    view: FindView,
    format: Format,
    sink: &mut Sink<'_>,
    conv: &mut Conversation<'_>,
) -> i32 {
    let result: io::Result<i32> = (|| {
        match format {
            Format::Paths => {
                for doc in &view.report.documents {
                    writeln!(sink.writer(), "{}", doc.path)?;
                }
                if view.report.truncated {
                    conv.line(&truncation_note(&view.report))?;
                }
            }
            Format::Json => render_find_json(sink.writer(), &view)?,
            Format::Jsonl => {
                for doc in &view.report.documents {
                    let line = project_json(
                        &find_doc_view(doc),
                        &view.cols,
                        view.all_cols,
                        DefaultCols::FrontmatterOnly,
                    );
                    writeln!(sink.writer(), "{}", serde_json::to_string(&line)?)?;
                }
                if view.report.truncated {
                    conv.line(&truncation_note(&view.report))?;
                }
            }
            Format::Records => {
                render_find_records(sink, &view)?;
            }
            Format::Markdown => unreachable!("find has no markdown format"),
        }
        warn_col_ignored(
            &view.cols,
            (format == Format::Paths).then_some("paths"),
            conv,
        )?;
        warn_unknown_cols_find(&view.report, &view.cols, conv)?;
        warn_unknown_sort_find(&view.report, view.sort_field.as_deref(), conv)?;
        Ok(EXIT_OK)
    })();

    render_outcome(result, conv.writer())
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
/// absent from every match (`warning:` prefix).
fn warn_unknown_cols_find(
    report: &FindReport,
    cols: &[String],
    conv: &mut Conversation<'_>,
) -> io::Result<()> {
    let (facets, fields) = split_cols(cols);
    for facet in &facets {
        if !KNOWN_FACETS.contains(&facet.as_str()) {
            conv.warning(&unknown_facet_message(facet))?;
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
            conv.warning(&format!(
                "--col field `{field}` not present in any matching document"
            ))?;
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
/// truncation false positive — a known defect that fires when the field is real
/// but beyond the `--limit` page, tracked as NRN-44; the fix lands there with
/// its ledger entry. Not part of this new-surface warning.)
fn warn_unknown_sort_find(
    report: &FindReport,
    sort_field: Option<&str>,
    conv: &mut Conversation<'_>,
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
        conv.warning(&format!(
            "--sort field `{field}` not present in any matching document"
        ))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::GlobalArgs;
    use crate::display::format::{FormatChoice, FormatSpec};
    use crate::display::Presenter;
    use crate::display::EXIT_OPERATIONAL;
    use crate::test_support::{global_args, FailingWriter};
    use serde_json::json;

    /// Drive `render_find` through the same resolution `emit` performs: a
    /// non-tty format, the `--color`-resolved palette, an 80-col sink.
    fn drive<O: Write, E: Write>(
        view: FindView,
        global: &GlobalArgs,
        presenter: &mut Presenter<O, E>,
    ) -> i32 {
        let format = view.format.resolve(false);
        let palette = crate::output::palette::resolve(global.color);
        let (out, err) = presenter.streams();
        let mut sink = Sink::new(out, &palette, 80);
        let mut conv = Conversation::new(err);
        render_find(view, format, &mut sink, &mut conv)
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
                has_diagnostic_errors: false,
            },
            cols: vec![],
            all_cols: false,
            sort_field: None,
            format: FormatChoice {
                explicit: Some(Format::Paths),
                spec: FormatSpec {
                    tty: Format::Records,
                    piped: Format::Paths,
                },
            },
        }
    }

    #[test]
    fn render_find_tolerates_broken_pipe() {
        let mut err = Vec::new();
        let global = global_args();
        let code = {
            let mut presenter = Presenter::new(FailingWriter(io::ErrorKind::BrokenPipe), &mut err);
            drive(find_view(), &global, &mut presenter)
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
            drive(find_view(), &global, &mut presenter)
        };
        assert_eq!(code, EXIT_OPERATIONAL);
        assert!(
            String::from_utf8(err).unwrap().starts_with("norn: "),
            "expected the norn: diagnostic"
        );
    }

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
            has_diagnostic_errors: false,
        };
        let mut err = Vec::new();
        let mut conv = Conversation::new(&mut err);
        warn_unknown_sort_find(&report, Some("priorty"), &mut conv).unwrap();
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
            has_diagnostic_errors: false,
        };
        let mut err = Vec::new();
        let mut conv = Conversation::new(&mut err);
        warn_unknown_sort_find(&report, Some("title"), &mut conv).unwrap();
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
            has_diagnostic_errors: false,
        };
        for structural in ["path", "stem"] {
            let mut err = Vec::new();
            let mut conv = Conversation::new(&mut err);
            warn_unknown_sort_find(&report, Some(structural), &mut conv).unwrap();
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
            has_diagnostic_errors: false,
        };
        let mut err = Vec::new();
        let mut conv = Conversation::new(&mut err);
        warn_unknown_sort_find(&report, Some("anything"), &mut conv).unwrap();
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
            has_diagnostic_errors: false,
        };
        let mut err = Vec::new();
        let mut conv = Conversation::new(&mut err);
        warn_unknown_sort_find(&report, Some("priorty"), &mut conv).unwrap();
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
            has_diagnostic_errors: false,
        };
        let mut err = Vec::new();
        let mut conv = Conversation::new(&mut err);
        warn_unknown_sort_find(&report, Some("priorty"), &mut conv).unwrap();
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
            has_diagnostic_errors: false,
        };
        let mut err = Vec::new();
        let mut conv = Conversation::new(&mut err);
        warn_unknown_sort_find(&report, None, &mut conv).unwrap();
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
            drive(view, &global, &mut presenter)
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
}
