//! Renderers for `norn count` output.

use crate::count::{CountOutput, GroupNode};
use std::collections::BTreeMap;
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
        CountOutput::GroupedMulti { by, total, groups } => {
            writeln!(s, "total      {}", total).unwrap();
            writeln!(s).unwrap();
            writeln!(s, "{}  count", by.join(" / ")).unwrap();
            render_group_tree(&mut s, groups, 0);
        }
    }
    s
}

/// Records-style nesting: each branch key on its own line, children indented
/// two spaces per level, leaf counts aligned within their sibling group.
fn render_group_tree(s: &mut String, groups: &BTreeMap<String, GroupNode>, depth: usize) {
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
                writeln!(s, "{indent}{key:<leaf_width$}  {count}").unwrap();
            }
            GroupNode::Branch(children) => {
                writeln!(s, "{indent}{key}").unwrap();
                render_group_tree(s, children, depth + 1);
            }
        }
    }
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
