//! `describe` (NRN-409).

use std::fmt::Write as _;
use std::io::{self, Write};

use norn_wire::DescribeReport;

use crate::display::conversation::Conversation;
use crate::display::emit::render_outcome;
use crate::display::format::Format;
use crate::display::output::DescribeView;
use crate::display::sink::Sink;
use crate::display::EXIT_OK;

pub(crate) fn render_describe(
    view: DescribeView,
    format: Format,
    sink: &mut Sink<'_>,
    conv: &mut Conversation<'_>,
) -> i32 {
    let text = match format {
        Format::Json => describe_json(&view.report),
        _ => describe_text(&view.report),
    };
    let result: io::Result<i32> = (|| {
        if text.ends_with('\n') {
            write!(sink.writer(), "{text}")?;
        } else {
            writeln!(sink.writer(), "{text}")?;
        }
        warn_unknown_by_describe(&view.report, &view.by, conv.writer())?;
        Ok(EXIT_OK)
    })();
    render_outcome(result, conv.writer())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::format::FormatSpec;
    use crate::display::Presenter;
    use crate::display::EXIT_OPERATIONAL;
    use crate::output::palette::Palette;
    use crate::test_support::FailingWriter;
    use norn_wire::{DataSummary, DateBounds, FieldDistribution, SkippedField, ValueCount};
    use serde_json::json;

    /// Drive `render_describe` through the same resolution `emit` performs —
    /// describe is unstyled, so a no-op palette sink.
    fn drive<O: Write, E: Write>(view: DescribeView, presenter: &mut Presenter<O, E>) -> i32 {
        let format = view.spec.resolve(view.explicit, false);
        let palette = Palette::off();
        let (out, err) = presenter.streams();
        let mut sink = Sink::new(out, &palette, 80);
        let mut conv = Conversation::new(err);
        render_describe(view, format, &mut sink, &mut conv)
    }

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
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["data"]["total"], 1164);
        assert_eq!(v["data"]["fields"][0]["values"][0]["value"], "note");
        assert!(text.starts_with(r#"{"folders":"#), "{text}");
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
            drive(describe_view(), &mut presenter)
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
            drive(describe_view(), &mut presenter)
        };
        assert_eq!(code, EXIT_OPERATIONAL);
        assert!(String::from_utf8(err).unwrap().starts_with("norn: "));
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
            drive(view, &mut presenter)
        };
        assert_eq!(code, EXIT_OK);
        assert!(!out.is_empty());
        assert!(String::from_utf8(err)
            .unwrap()
            .contains("--by field `priorty`"));
    }
}
