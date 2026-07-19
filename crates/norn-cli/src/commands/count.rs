//! `norn count` — total, single-field distribution, or nested group tree.
//!
//! The command module maps its clap `Args` (`--by`, the shared filter surface,
//! `--format`) into [`CountParams`], summons the vault owner, and renders the
//! [`CountReport`] byte-faithfully to the donor (`src/count/render.rs`):
//! `--format text` (the default) is padded columns; `--format json` is the
//! untagged compact serialization.

use std::fmt::Write as _;
use std::io::Write;

use norn_wire::{CountParams, CountReport, GroupNode};

use crate::cli::{CountArgs, CountFormat, GlobalArgs};
use crate::display::{Presenter, EXIT_OK, EXIT_OPERATIONAL};

/// Present the command's outcome and return the process exit code.
pub fn run<O: Write, E: Write>(
    args: &CountArgs,
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

    let params = CountParams {
        by: args.by.clone(),
        filter: args.filters.to_params(),
    };
    let report = match session.count(params) {
        Ok(r) => r,
        Err(e) => {
            presenter.present_diagnostic(&crate::routed::client_error_diagnostic(&e));
            return EXIT_OPERATIONAL;
        }
    };

    let text = match args.format {
        CountFormat::Json => render_json(&report),
        CountFormat::Text => render_text(&report),
    };
    let out = presenter.out();
    // Exactly one trailing newline (donor `emit`).
    if text.ends_with('\n') {
        let _ = write!(out, "{text}");
    } else {
        let _ = writeln!(out, "{text}");
    }
    EXIT_OK
}

fn render_json(report: &CountReport) -> String {
    serde_json::to_string(report).unwrap_or_else(|_| "{}".to_string())
}

fn render_text(report: &CountReport) -> String {
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
            render_group_tree(&mut s, groups, 0);
        }
    }
    s
}

fn render_group_tree(
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
                render_group_tree(s, children, depth + 1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn total_text_is_padded() {
        let r = CountReport::Total { total: 42 };
        assert_eq!(render_text(&r), "total      42\n");
    }

    #[test]
    fn grouped_text_columns_align() {
        let mut groups = BTreeMap::new();
        groups.insert("active".to_string(), 12usize);
        groups.insert("backlog".to_string(), 11usize);
        let r = CountReport::Grouped {
            by: "status".to_string(),
            total: 23,
            groups,
        };
        assert_eq!(
            render_text(&r),
            "total      23\n\nstatus   count\nactive   12\nbacklog  11\n"
        );
    }

    #[test]
    fn total_json_is_compact() {
        let r = CountReport::Total { total: 7 };
        assert_eq!(render_json(&r), r#"{"total":7}"#);
    }

    #[test]
    fn grouped_json_field_order_is_by_total_groups() {
        let mut groups = BTreeMap::new();
        groups.insert("active".to_string(), 3usize);
        let r = CountReport::Grouped {
            by: "status".to_string(),
            total: 3,
            groups,
        };
        assert_eq!(
            render_json(&r),
            r#"{"by":"status","total":3,"groups":{"active":3}}"#
        );
    }
}
