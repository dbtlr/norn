//! MigrationPlan rendering for `norn repair --plan` (Report / Paths formats).
//!
//! `norn repair --plan` produces a unified `MigrationPlan`; these renderers
//! present it as a human summary (`report`) or one affected path per line
//! (`paths`). The `json` format is handled directly by the dispatcher via
//! `serde_json::to_string_pretty`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

use crate::migration_plan::{MigrationOp, MigrationPlan};
use anyhow::Result;

use crate::cli::{ColorWhen, RepairArgs};
use crate::output::palette;
use crate::output::primitives::{status_headline, tally_group};
use crate::repair::skip_reasons::prose_for;

/// Extract the affected vault-relative paths from a single operation.
///
/// Frontmatter / link ops carry a single `path`; structural moves carry
/// `src` + `dst`. Any field present is collected so `paths` format reflects
/// every file an op touches.
fn op_paths(op: &MigrationOp) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(obj) = op.fields.as_object() {
        for key in ["path", "src", "dst", "destination"] {
            if let Some(v) = obj.get(key).and_then(|v| v.as_str()) {
                out.push(v.to_string());
            }
        }
    }
    out
}

pub fn write_report(plan: &MigrationPlan, args: &RepairArgs) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let p = palette::resolve(ColorWhen::Auto);

    // Header: "Repair plan against <vault_root>…"
    let title = format!("Repair plan against {}", plan.vault_root);
    status_headline(&mut out, &p, &title)?;
    writeln!(out)?;

    // Count line: "<ops> operations proposed across <files> files"
    let n_ops = plan.operations.len();
    let n_files: usize = plan
        .operations
        .iter()
        .flat_map(op_paths)
        .collect::<BTreeSet<_>>()
        .len();
    writeln!(out, "  {n_ops} operations proposed across {n_files} files")?;

    // Operations grouped by kind — suppressed when there are no operations.
    if n_ops > 0 {
        let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
        for op in &plan.operations {
            *by_kind.entry(op.kind.as_str()).or_insert(0) += 1;
        }
        let label_strings: Vec<(String, usize)> = by_kind
            .iter()
            .map(|(kind, &count)| ((*kind).to_string(), count))
            .collect();
        let rows: Vec<(&str, usize)> = label_strings
            .iter()
            .map(|(label, count)| (label.as_str(), *count))
            .collect();
        tally_group(&mut out, &p, "Operations by kind", &rows, 80)?;
    }
    writeln!(out)?;

    // Footnotes — one line per op that carries one.
    let footnotes: Vec<&String> = plan
        .operations
        .iter()
        .filter_map(|op| op.footnote.as_ref())
        .collect();
    if !footnotes.is_empty() {
        let header = format!("Footnotes ({})", footnotes.len());
        status_headline(&mut out, &p, &header)?;
        for note in &footnotes {
            writeln!(out, "  {note}")?;
        }
        writeln!(out)?;
    }

    // Skipped tally — grouped by reason code with prose.
    if !plan.skipped.is_empty() {
        let mut by_reason: BTreeMap<&str, usize> = BTreeMap::new();
        for sf in &plan.skipped {
            *by_reason.entry(sf.reason.as_str()).or_insert(0) += 1;
        }
        let header = format!("Skipped ({})", plan.skipped.len());
        let label_strings: Vec<(String, usize)> = by_reason
            .iter()
            .map(|(code, &count)| (format!("{}  {}", code, prose_for(code)), count))
            .collect();
        let rows: Vec<(&str, usize)> = label_strings
            .iter()
            .map(|(label, count)| (label.as_str(), *count))
            .collect();
        tally_group(&mut out, &p, &header, &rows, 80)?;
        writeln!(out)?;
    }

    // Top affected files
    const TOP_FILES_N: usize = 5;
    if n_ops > 0 {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for op in &plan.operations {
            for path in op_paths(op) {
                *counts.entry(path).or_insert(0) += 1;
            }
        }
        if !counts.is_empty() {
            // Sort: count desc, then path asc (BTreeMap gives path-asc order).
            let mut sorted: Vec<(String, usize)> = counts.into_iter().collect();
            sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            sorted.truncate(TOP_FILES_N);

            let rows: Vec<(&str, usize)> = sorted
                .iter()
                .map(|(label, count)| (label.as_str(), *count))
                .collect();
            tally_group(&mut out, &p, "Top affected files", &rows, 80)?;
            writeln!(out)?;
        }
    }

    // Apply guidance — filter-aware.
    let active_filter_args = collect_active_filter_flags(args);
    let confidence_already_active = active_filter_args.iter().any(|f| f == "--confidence");
    let skip_reason_active = active_filter_args.iter().any(|f| f == "--skip-reason");

    let has_anything_actionable = n_ops > 0 || !plan.skipped.is_empty();
    if has_anything_actionable {
        writeln!(out, "  To inspect proposed changes")?;
        if !confidence_already_active {
            let mut high_conf_args = active_filter_args.clone();
            high_conf_args.push("--confidence".into());
            high_conf_args.push("high".into());
            let inspect_high = build_command(&high_conf_args, &["--format", "json"]);
            writeln!(out, "    {inspect_high}")?;
        }
        let inspect_unfiltered = build_command(&active_filter_args, &["--format", "json"]);
        writeln!(out, "    {inspect_unfiltered}")?;
        writeln!(out)?;
    }

    if !skip_reason_active && n_ops > 0 {
        let apply_args = if confidence_already_active {
            active_filter_args.clone()
        } else {
            let mut v = active_filter_args.clone();
            v.push("--confidence".into());
            v.push("high".into());
            v
        };
        let cmd = build_command(&apply_args, &["--format", "json"]);
        writeln!(out, "  To apply")?;
        writeln!(out, "    {cmd} | norn apply -")?;
        writeln!(out)?;
    }

    Ok(())
}

fn collect_active_filter_flags(args: &RepairArgs) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    if let Some(c) = args.confidence {
        out.push("--confidence".into());
        let value = match c {
            crate::cli::ConfidenceArg::High => "high",
        };
        out.push(value.into());
    }

    for pat in &args.skip_reason {
        out.push("--skip-reason".into());
        out.push(quote_if_glob(pat));
    }

    for pat in &args.triage.code {
        out.push("--code".into());
        out.push(quote_if_glob(pat));
    }

    for pat in &args.triage.severity {
        out.push("--severity".into());
        out.push(quote_if_glob(pat));
    }

    for pat in &args.triage.field {
        out.push("--field".into());
        out.push(quote_if_glob(pat));
    }

    for pat in &args.triage.rule {
        out.push("--rule".into());
        out.push(quote_if_glob(pat));
    }

    for pat in &args.triage.path {
        out.push("--path".into());
        out.push(quote_if_glob(pat));
    }

    for pat in &args.triage.target {
        out.push("--target".into());
        out.push(quote_if_glob(pat));
    }

    for pat in &args.triage.reason {
        out.push("--reason".into());
        out.push(quote_if_glob(pat));
    }

    out
}

fn quote_if_glob(s: &str) -> String {
    if s.contains('*') || s.contains('?') || s.contains('[') {
        format!("'{s}'")
    } else {
        s.to_string()
    }
}

fn build_command(filter_flags: &[String], trailing: &[&str]) -> String {
    let mut parts: Vec<String> = vec!["norn".into(), "repair".into(), "--plan".into()];
    parts.extend(filter_flags.iter().cloned());
    parts.extend(trailing.iter().map(|s| s.to_string()));
    parts.join(" ")
}

pub fn write_paths(plan: &MigrationPlan) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let paths: BTreeSet<String> = plan.operations.iter().flat_map(op_paths).collect();
    for path in paths {
        writeln!(out, "{path}")?;
    }
    Ok(())
}
