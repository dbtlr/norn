//! Renderers for `vault count` output.

use crate::count::CountOutput;
use std::fmt::Write;

pub fn render_text(out: &CountOutput) -> String {
    let mut s = String::new();
    match out {
        CountOutput::Total { total } => {
            writeln!(s, "total      {}", total).unwrap();
        }
        CountOutput::Grouped { by, total, groups } => {
            writeln!(s, "total      {}", total).unwrap();
            writeln!(s).unwrap();
            let header_width = by
                .len()
                .max(groups.keys().map(String::len).max().unwrap_or(0));
            writeln!(s, "{:<width$}  count", by, width = header_width).unwrap();
            for (key, count) in groups {
                writeln!(s, "{:<width$}  {}", key, count, width = header_width).unwrap();
            }
        }
    }
    s
}

pub fn render_json(out: &CountOutput) -> String {
    serde_json::to_string(out).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn total_only_text() {
        let out = CountOutput::Total { total: 42 };
        let s = render_text(&out);
        assert!(s.contains("total      42"));
    }

    #[test]
    fn grouped_text_columns_align() {
        let groups: BTreeMap<String, usize> =
            [("active".to_string(), 1), ("backlog".to_string(), 17)]
                .into_iter()
                .collect();
        let out = CountOutput::Grouped {
            by: "status".to_string(),
            total: 18,
            groups,
        };
        let s = render_text(&out);
        assert!(s.contains("total      18"));
        assert!(s.contains("status"));
        assert!(s.contains("active"));
        assert!(s.contains("backlog"));
        assert!(s.contains("17"));
    }

    #[test]
    fn grouped_json_shape() {
        let groups: BTreeMap<String, usize> = [("active".to_string(), 1)].into_iter().collect();
        let out = CountOutput::Grouped {
            by: "status".to_string(),
            total: 1,
            groups,
        };
        let s = render_json(&out);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["by"], "status");
        assert_eq!(v["total"], 1);
        assert_eq!(v["groups"]["active"], 1);
    }

    #[test]
    fn total_only_json_shape() {
        let out = CountOutput::Total { total: 7 };
        let s = render_json(&out);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["total"], 7);
        assert!(v.get("by").is_none());
    }
}
