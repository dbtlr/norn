//! `count` (NRN-409).

use std::fmt::Write as _;
use std::io::{self, Write};

use norn_wire::{CountReport, GroupNode};

use crate::display::conversation::Conversation;
use crate::display::emit::render_outcome;
use crate::display::format::Format;
use crate::display::output::CountView;
use crate::display::sink::Sink;
use crate::display::EXIT_OK;

pub(crate) fn render_count(
    view: CountView,
    format: Format,
    sink: &mut Sink<'_>,
    conv: &mut Conversation<'_>,
) -> i32 {
    let text = match format {
        Format::Json => count_json(&view.report),
        _ => count_text(&view.report),
    };
    let result: io::Result<i32> = (|| {
        if text.ends_with('\n') {
            write!(sink.writer(), "{text}")?;
        } else {
            writeln!(sink.writer(), "{text}")?;
        }
        warn_unknown_by_count(&view.report, conv.writer())?;
        Ok(EXIT_OK)
    })();
    render_outcome(result, conv.writer())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::format::{FormatChoice, FormatSpec};
    use crate::display::Presenter;
    use crate::display::EXIT_OPERATIONAL;
    use crate::output::palette::Palette;
    use crate::test_support::FailingWriter;
    use std::collections::BTreeMap;

    /// Drive `render_count` through the same resolution `emit` performs — count
    /// is unstyled, so a no-op palette sink.
    fn drive<O: Write, E: Write>(view: CountView, presenter: &mut Presenter<O, E>) -> i32 {
        let format = view.format.resolve(false);
        let palette = Palette::off();
        let (out, err) = presenter.streams();
        let mut sink = Sink::new(out, &palette, 80);
        let mut conv = Conversation::new(err);
        render_count(view, format, &mut sink, &mut conv)
    }

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

    fn count_view(explicit: Format) -> CountView {
        CountView {
            report: CountReport::Total { total: 3 },
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
    fn render_count_tolerates_broken_pipe() {
        let mut err = Vec::new();
        let code = {
            let mut presenter = Presenter::new(FailingWriter(io::ErrorKind::BrokenPipe), &mut err);
            drive(count_view(Format::Json), &mut presenter)
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
            drive(count_view(Format::Json), &mut presenter)
        };
        assert_eq!(code, EXIT_OPERATIONAL);
        assert!(String::from_utf8(err).unwrap().starts_with("norn: "));
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
            format: FormatChoice {
                explicit: Some(Format::Json),
                spec: FormatSpec {
                    tty: Format::Records,
                    piped: Format::Records,
                },
            },
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut presenter = Presenter::new(&mut out, &mut err);
            drive(view, &mut presenter)
        };
        assert_eq!(code, EXIT_OK);
        assert!(!out.is_empty(), "the count still renders its normal output");
        assert!(String::from_utf8(err)
            .unwrap()
            .contains("--by field `priorty`"));
    }
}
