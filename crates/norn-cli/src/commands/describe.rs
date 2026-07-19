//! `norn describe` — the vault at a glance: structure (folders + declared rules
//! + inbox + schema) and, with `--data`/`--stats`, a contents-summary.
//!
//! The command maps its clap `Args` into [`DescribeParams`], summons the owner
//! (which serves the structure from its retained config and the data summary
//! from the warm cache), and renders the [`DescribeReport`] byte-faithfully to
//! the donor (`src/describe/render.rs`): `--format text` (default) is the
//! count/summary block; `--format json` is the whole struct serialized compact.

use std::fmt::Write as _;
use std::io::Write;

use norn_wire::{DescribeParams, DescribeReport};

use crate::cli::{DescribeArgs, DescribeFormat, GlobalArgs};
use crate::display::{Presenter, EXIT_OK, EXIT_OPERATIONAL};

/// Present the command's outcome and return the process exit code.
pub fn run<O: Write, E: Write>(
    args: &DescribeArgs,
    global: &GlobalArgs,
    presenter: &mut Presenter<O, E>,
) -> i32 {
    let mut session = match crate::routed::open_session(global) {
        Ok(s) => s,
        Err(diag) => {
            presenter.present_diagnostic(&diag);
            return EXIT_OPERATIONAL;
        }
    };

    let params = DescribeParams {
        // `--stats` is a pure alias for `--data`; `--by` implies data verb-side.
        data: args.data || args.stats,
        by: args.by.clone(),
        limit: args.limit,
        filter: args.filters.to_params(),
    };

    let report = match session.describe(params) {
        Ok(r) => r,
        Err(e) => {
            presenter.present_diagnostic(&crate::routed::client_error_diagnostic(&e));
            return EXIT_OPERATIONAL;
        }
    };

    let text = match args.format.unwrap_or(DescribeFormat::Text) {
        DescribeFormat::Json => render_json(&report),
        DescribeFormat::Text => render_text(&report),
    };
    let out = presenter.out();
    // Exactly one trailing newline (donor CLI): the text renderer already ends
    // its lines with `\n` (empty vault → a bare `\n`); compact JSON has none, so
    // one is appended.
    if text.ends_with('\n') {
        let _ = write!(out, "{text}");
    } else {
        let _ = writeln!(out, "{text}");
    }
    EXIT_OK
}

/// `--format json`: the whole report serialized compact (donor `render_json`).
fn render_json(report: &DescribeReport) -> String {
    serde_json::to_string(report).unwrap_or_else(|_| "{}".to_string())
}

/// `--format text`: the structure counts, then the data summary when present
/// (donor `render_text`).
fn render_text(report: &DescribeReport) -> String {
    let mut s = String::new();
    // ── Structure (counts only; schema is JSON-only) ─────────────────────
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
    // ── Data summary ─────────────────────────────────────────────────────
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
    use norn_wire::{DataSummary, DateBounds, FieldDistribution, SkippedField, ValueCount};

    fn sample() -> DescribeReport {
        DescribeReport {
            folders: vec!["".into(), "notes".into()],
            path_rules: vec![],
            creatable_rules: vec![],
            inbox: None,
            schema: serde_json::json!({}),
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
    fn text_renders_structure_then_data() {
        let s = render_text(&sample());
        assert!(s.contains("folders    2"), "{s}");
        assert!(
            s.contains("1164 documents · created 2026-05-10 → 2026-07-03"),
            "{s}"
        );
        assert!(s.contains("type: note 575 · task 420 (+4 more)"), "{s}");
        assert!(s.contains("(skipped: title 1164/1164)"), "{s}");
    }

    #[test]
    fn structure_only_text_has_no_data_block() {
        let mut report = sample();
        report.data = None;
        let s = render_text(&report);
        assert_eq!(s, "folders    2\n");
    }

    #[test]
    fn json_serializes_the_whole_report() {
        let json = render_json(&sample());
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["data"]["total"], 1164);
        assert_eq!(v["data"]["fields"][0]["values"][0]["value"], "note");
        // Top-level field order is preserved (struct serialization, not sorted).
        assert!(json.starts_with(r#"{"folders":"#), "{json}");
    }
}
