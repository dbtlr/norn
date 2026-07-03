//! TTY + JSON renderers for `norn describe`.
use std::fmt::Write;

use crate::describe::DescribeOutput;

pub fn render_text(out: &DescribeOutput) -> String {
    let mut s = String::new();
    // ── Structure ────────────────────────────────────────────────────────
    if !out.folders.is_empty() {
        writeln!(s, "folders    {}", out.folders.len()).unwrap();
    }
    if !out.path_rules.is_empty() {
        writeln!(s, "path rules {}", out.path_rules.len()).unwrap();
    }
    if !out.creatable_rules.is_empty() {
        writeln!(s, "creatable  {}", out.creatable_rules.len()).unwrap();
    }
    if let Some(inbox) = &out.inbox {
        writeln!(s, "inbox      {inbox}").unwrap();
    }
    // ── Data summary ─────────────────────────────────────────────────────
    if let Some(data) = &out.data {
        if !s.is_empty() {
            writeln!(s).unwrap();
        }
        let dates = data
            .dates
            .iter()
            .map(|d| format!("{} {} → {}", d.field, d.min, d.max))
            .collect::<Vec<_>>()
            .join(" · ");
        if dates.is_empty() {
            writeln!(s, "{} documents", data.total).unwrap();
        } else {
            writeln!(s, "{} documents · {}", data.total, dates).unwrap();
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
            writeln!(s, "{:<label_width$} {body}{more}", format!("{}:", f.field)).unwrap();
        }
        if !data.skipped.is_empty() {
            let sk = data
                .skipped
                .iter()
                .map(|s| format!("{} {}/{}", s.field, s.distinct, s.total))
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(s, "(skipped: {sk})").unwrap();
        }
    }
    s
}

pub fn render_json(out: &DescribeOutput) -> String {
    serde_json::to_string(out).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::describe::data::{
        DataSummary, DateBounds, FieldDistribution, SkippedField, ValueCount,
    };

    fn sample() -> DescribeOutput {
        DescribeOutput {
            folders: vec![],
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
                    more: 0,
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
    fn text_renders_total_distribution_dates_and_skipped() {
        let s = render_text(&sample());
        assert!(s.contains("1164 documents"));
        assert!(s.contains("created 2026-05-10 → 2026-07-03"));
        assert!(s.contains("type:"));
        assert!(s.contains("note 575"));
        assert!(s.contains("skipped: title 1164/1164"));
    }

    #[test]
    fn text_renders_more_suffix() {
        let mut o = sample();
        if let Some(d) = &mut o.data {
            d.fields[0].more = 4;
        }
        let s = render_text(&o);
        assert!(s.contains("(+4 more)"), "got: {s}");
    }

    #[test]
    fn json_roundtrips_data_arrays() {
        let s = render_json(&sample());
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["data"]["total"], 1164);
        assert_eq!(v["data"]["fields"][0]["field"], "type");
        assert_eq!(v["data"]["fields"][0]["values"][0]["value"], "note");
        assert_eq!(v["data"]["dates"][0]["field"], "created");
    }
}
