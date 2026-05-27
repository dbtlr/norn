//! Repair-plan rendering (Report / Paths formats).
//!
//! Report and Paths renderers stubbed by Task 7; bodies land in Tasks 8–13.

use std::collections::BTreeSet;
use std::io::Write;

use crate::standards::{Confidence, RepairPlan};
use anyhow::Result;

use crate::cli::{ColorWhen, RepairPlanArgs};
use crate::output::palette;
use crate::output::primitives::{status_headline, tally_group};
use crate::repair::skip_reasons::prose_for;

pub fn write_report(plan: &RepairPlan, args: &RepairPlanArgs) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let p = palette::resolve(ColorWhen::Auto);

    // Header: "Repair plan against <vault_root>…"
    let title = format!("Repair plan against {}", plan.vault_root);
    status_headline(&mut out, &p, &title)?;
    writeln!(out)?;

    // Count line: "  <findings> findings analyzed → <changes> changes proposed across <files> files"
    let n_changes = plan.changes.len();
    let n_files = plan
        .changes
        .iter()
        .map(|c| &c.path)
        .collect::<BTreeSet<_>>()
        .len();
    let count_text = format!(
        "{} findings analyzed \u{2192} {} changes proposed across {} files",
        plan.summary.findings, n_changes, n_files
    );
    writeln!(out, "  {count_text}")?;

    // Confidence breakdown — only when there are changes and at least one footnote.
    // Each band line is suppressed when its count is zero (records doctrine).
    if n_changes > 0 {
        let (n_high, n_medium) = count_confidence(plan);
        if n_high > 0 {
            writeln!(
                out,
                "    {n_high} high    (slug-identity or near-zero edit distance)"
            )?;
        }
        if n_medium > 0 {
            writeln!(
                out,
                "    {n_medium} medium  (Levenshtein ratio \u{2265} 0.7)"
            )?;
        }
    }
    writeln!(out)?;

    // Skipped tally
    if plan.summary.skipped.total > 0 {
        let header = format!("Skipped ({})", plan.summary.skipped.total);
        // Build owned label strings combining code and prose, then borrow them as &str rows.
        let label_strings: Vec<(String, usize)> = plan
            .summary
            .skipped
            .by_reason
            .iter()
            .map(|(code, &count)| {
                let label = format!("{}  {}", code, prose_for(code));
                (label, count)
            })
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
    if !plan.changes.is_empty() {
        // Aggregate changes per path.
        let mut counts: std::collections::BTreeMap<&camino::Utf8Path, usize> =
            std::collections::BTreeMap::new();
        for change in &plan.changes {
            *counts.entry(change.path.as_ref()).or_insert(0) += 1;
        }
        // Sort: count desc, then path asc (BTreeMap already gives path-asc order,
        // so the then_with is belt-and-suspenders for when counts differ).
        let mut sorted: Vec<(&camino::Utf8Path, usize)> = counts.into_iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        sorted.truncate(TOP_FILES_N);

        let label_strings: Vec<(String, usize)> = sorted
            .iter()
            .map(|(path, count)| (path.to_string(), *count))
            .collect();
        let rows: Vec<(&str, usize)> = label_strings
            .iter()
            .map(|(label, count)| (label.as_str(), *count))
            .collect();
        tally_group(&mut out, &p, "Top affected files", &rows, 80)?;
        writeln!(out)?;
    }

    // Apply guidance — filter-aware
    let active_filter_args = collect_active_filter_flags(args);
    let confidence_already_active = active_filter_args.iter().any(|f| f == "--confidence");
    let skip_reason_active = active_filter_args.iter().any(|f| f == "--skip-reason");

    let has_anything_actionable = !plan.changes.is_empty() || plan.summary.skipped.total > 0;
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

    if !skip_reason_active && !plan.changes.is_empty() {
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
        writeln!(out, "    {cmd} | vault repair apply --dry-run")?;
        writeln!(out, "    {cmd} | vault repair apply")?;
        writeln!(out)?;
    }

    Ok(())
}

fn collect_active_filter_flags(args: &RepairPlanArgs) -> Vec<String> {
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
    let mut parts: Vec<String> = vec!["vault".into(), "repair".into(), "plan".into()];
    parts.extend(filter_flags.iter().cloned());
    parts.extend(trailing.iter().map(|s| s.to_string()));
    parts.join(" ")
}

fn count_confidence(plan: &RepairPlan) -> (usize, usize) {
    let mut high = 0usize;
    let mut medium = 0usize;
    for footnote in &plan.footnotes {
        match footnote.confidence {
            Confidence::High => high += 1,
            Confidence::Medium => medium += 1,
        }
    }
    (high, medium)
}

pub fn write_paths(plan: &RepairPlan) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let paths: BTreeSet<&camino::Utf8Path> = plan.changes.iter().map(|c| c.path.as_ref()).collect();
    for path in paths {
        writeln!(out, "{path}")?;
    }
    Ok(())
}
