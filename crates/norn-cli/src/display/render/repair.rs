//! `repair` (findings → plan; read-only) (NRN-409).
//!
//! Bare `norn repair` prints the findings summary (or, with `--format json`, the
//! report-envelope JSON of that same summary); `--plan` emits the
//! `MigrationPlan` (records / json / paths) and/or writes it to `--out`. The exit
//! code is `has_diagnostic_errors` for both, independent of the triage filters
//! (a `--code` narrow never masks a vault error).

use std::io::{self, Write};

use norn_wire::MigrationPlan;

use crate::cli::RepairPlanFormat;
use crate::display::conversation::Conversation;
use crate::display::emit::render_outcome;
use crate::display::output::RepairView;
use crate::display::sink::Sink;
use crate::display::{EXIT_OK, EXIT_OPERATIONAL};
use crate::output::glyphs;

pub(crate) fn render_repair(
    view: RepairView,
    is_tty: bool,
    sink: &mut Sink<'_>,
    conv: &mut Conversation<'_>,
) -> i32 {
    let ascii = glyphs::use_ascii();

    // The plan now crosses the wire TYPED (NRN-405 part b) — no parse, no
    // undecodable-frame branch.
    let plan = &view.report.plan;

    let result: io::Result<i32> = (|| {
        if view.plan {
            emit_repair_plan(sink, plan, &view, ascii, is_tty)?;
        } else if matches!(view.format, Some(RepairPlanFormat::Json)) {
            // `--format json` returns the JSON of what bare `repair` prints
            // WITHOUT it: the findings-summary envelope, never the plan.
            write_repair_summary_json(sink.writer(), &view.report, plan)?;
        } else {
            write_repair_summary(sink.writer(), &view.report, plan)?;
        }
        Ok(if view.report.has_diagnostic_errors {
            EXIT_OPERATIONAL
        } else {
            EXIT_OK
        })
    })();

    render_outcome(result, conv.writer())
}

/// Bare `norn repair`: a read-only findings summary — total findings across the
/// vault, a per-code tally, and how many the planner would turn into operations
/// vs. skip.
fn write_repair_summary(
    out: &mut dyn Write,
    report: &norn_wire::RepairReport,
    plan: &MigrationPlan,
) -> io::Result<()> {
    writeln!(
        out,
        "{} findings across {} documents",
        report.findings_total, report.total_docs
    )?;
    for (code, count) in &report.findings_by_code {
        writeln!(out, "  {count}  {code}")?;
    }
    writeln!(out)?;
    let ops = plan.operations.len();
    let skipped = plan.skipped.len();
    writeln!(out, "{ops} repairable as operations, {skipped} skipped")?;
    if ops > 0 || skipped > 0 {
        writeln!(out)?;
        writeln!(out, "Run `norn repair --plan` to generate a MigrationPlan.")?;
        writeln!(out, "Pipe it into `norn apply -` to apply.")?;
    }
    Ok(())
}

/// Bare `norn repair --format json`: the JSON representation of the same
/// findings summary the prose form prints — the report envelope (findings total,
/// documents scanned, the per-code tally, and the planner's repairable/skipped
/// counts). Never the `MigrationPlan` (that is `--plan --format json`).
fn write_repair_summary_json(
    out: &mut dyn Write,
    report: &norn_wire::RepairReport,
    plan: &MigrationPlan,
) -> io::Result<()> {
    let envelope = serde_json::json!({
        "findings_total": report.findings_total,
        "total_docs": report.total_docs,
        "findings_by_code": report.findings_by_code,
        "operations": plan.operations.len(),
        "skipped": plan.skipped.len(),
    });
    // A summary envelope of scalars and a small tally always serializes.
    let json = serde_json::to_string_pretty(&envelope)
        .expect("repair summary envelope must always serialize");
    writeln!(out, "{json}")
}

/// `norn repair --plan`: `--out` writes the JSON plan to a file (independent of
/// `--format`); `--format` governs stdout (records / json / paths), silent when
/// `--out` is set without `--format`.
fn emit_repair_plan(
    sink: &mut Sink<'_>,
    plan: &MigrationPlan,
    view: &RepairView,
    ascii: bool,
    is_tty: bool,
) -> io::Result<()> {
    // The verbatim `--format json` / `--out` bytes: the plan crosses typed now, so
    // the CLI serializes it here — the same `to_string_pretty` over the same plan,
    // so the bytes match what the owner would emit. Serialization of an
    // in-memory MigrationPlan cannot fail.
    let plan_json = serde_json::to_string_pretty(plan)
        .expect("MigrationPlan must always serialize (matches canonical_hash)");

    // --out: always writes JSON (the pretty plan bytes), independent of --format.
    if let Some(path) = &view.out {
        std::fs::write(path, format!("{plan_json}\n"))?;
    }

    let stdout_format = if view.format.is_none() && view.out.is_some() {
        None
    } else {
        Some(view.format.unwrap_or({
            if is_tty {
                RepairPlanFormat::Records
            } else {
                RepairPlanFormat::Json
            }
        }))
    };

    match stdout_format {
        Some(RepairPlanFormat::Records) => {
            write_repair_report(sink, plan, &view.filter_flags, ascii)?
        }
        Some(RepairPlanFormat::Json) => {
            // The plan's `to_string_pretty` bytes, verbatim, with one newline.
            writeln!(sink.writer(), "{plan_json}")?;
        }
        Some(RepairPlanFormat::Paths) => write_repair_paths(sink.writer(), plan)?,
        None => {}
    }
    Ok(())
}

/// The vault-relative paths a single op touches — frontmatter/link ops carry
/// `path`; structural moves carry `src`/`dst`.
fn repair_op_paths(op: &norn_wire::MigrationOp) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(obj) = op.fields.as_object() {
        for key in ["path", "src", "dst", "destination"] {
            if let Some(v) = obj.get(key).and_then(|v| v.as_str()) {
                paths.push(v.to_string());
            }
        }
    }
    paths
}

/// `--format paths`: every affected vault-relative path, sorted + deduplicated,
/// one per line.
fn write_repair_paths(out: &mut dyn Write, plan: &MigrationPlan) -> io::Result<()> {
    let paths: std::collections::BTreeSet<String> =
        plan.operations.iter().flat_map(repair_op_paths).collect();
    for path in paths {
        writeln!(out, "{path}")?;
    }
    Ok(())
}

/// `--format records`: the human decision-support summary — a headline, an
/// operations-by-kind tally, footnotes, a skipped-by-reason tally, top affected
/// files, and filter-aware apply guidance.
fn write_repair_report(
    sink: &mut Sink<'_>,
    plan: &MigrationPlan,
    filter_flags: &[String],
    ascii: bool,
) -> io::Result<()> {
    use std::collections::{BTreeMap, BTreeSet};

    sink.status_headline(&format!("Repair plan against {}", plan.vault_root), ascii)?;
    writeln!(sink.writer())?;

    let n_ops = plan.operations.len();
    let n_files: usize = plan
        .operations
        .iter()
        .flat_map(repair_op_paths)
        .collect::<BTreeSet<_>>()
        .len();
    writeln!(
        sink.writer(),
        "  {n_ops} operations proposed across {n_files} files"
    )?;

    if n_ops > 0 {
        let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
        for op in &plan.operations {
            *by_kind.entry(op.kind.as_str()).or_insert(0) += 1;
        }
        let rows: Vec<(&str, usize)> = by_kind.into_iter().collect();
        sink.tally_group("Operations by kind", &rows, ascii)?;
    }
    writeln!(sink.writer())?;

    let footnotes: Vec<&String> = plan
        .operations
        .iter()
        .filter_map(|op| op.footnote.as_ref())
        .collect();
    if !footnotes.is_empty() {
        sink.status_headline(&format!("Footnotes ({})", footnotes.len()), ascii)?;
        for note in &footnotes {
            writeln!(sink.writer(), "  {note}")?;
        }
        writeln!(sink.writer())?;
    }

    if !plan.skipped.is_empty() {
        let mut by_reason: BTreeMap<&str, usize> = BTreeMap::new();
        for sf in &plan.skipped {
            *by_reason.entry(sf.reason.as_str()).or_insert(0) += 1;
        }
        let labels: Vec<(String, usize)> = by_reason
            .iter()
            .map(|(code, &count)| (format!("{code}  {}", skip_reason_prose(code)), count))
            .collect();
        let rows: Vec<(&str, usize)> = labels.iter().map(|(l, c)| (l.as_str(), *c)).collect();
        sink.tally_group(&format!("Skipped ({})", plan.skipped.len()), &rows, ascii)?;
        writeln!(sink.writer())?;
    }

    const TOP_FILES_N: usize = 5;
    if n_ops > 0 {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for op in &plan.operations {
            for path in repair_op_paths(op) {
                *counts.entry(path).or_insert(0) += 1;
            }
        }
        if !counts.is_empty() {
            let mut sorted: Vec<(String, usize)> = counts.into_iter().collect();
            sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            sorted.truncate(TOP_FILES_N);
            let rows: Vec<(&str, usize)> = sorted.iter().map(|(l, c)| (l.as_str(), *c)).collect();
            sink.tally_group("Top affected files", &rows, ascii)?;
            writeln!(sink.writer())?;
        }
    }

    // Apply guidance — filter-aware.
    let confidence_active = filter_flags.iter().any(|f| f == "--confidence");
    let skip_reason_active = filter_flags.iter().any(|f| f == "--skip-reason");
    let has_actionable = n_ops > 0 || !plan.skipped.is_empty();
    if has_actionable {
        writeln!(sink.writer(), "  To inspect proposed changes")?;
        if !confidence_active {
            let mut high = filter_flags.to_vec();
            high.push("--confidence".into());
            high.push("high".into());
            writeln!(
                sink.writer(),
                "    {}",
                repair_command(&high, &["--format", "json"])
            )?;
        }
        writeln!(
            sink.writer(),
            "    {}",
            repair_command(filter_flags, &["--format", "json"])
        )?;
        writeln!(sink.writer())?;
    }
    if !skip_reason_active && n_ops > 0 {
        let apply_flags = if confidence_active {
            filter_flags.to_vec()
        } else {
            let mut v = filter_flags.to_vec();
            v.push("--confidence".into());
            v.push("high".into());
            v
        };
        writeln!(sink.writer(), "  To apply")?;
        writeln!(
            sink.writer(),
            "    {} | norn apply -",
            repair_command(&apply_flags, &["--format", "json"])
        )?;
        writeln!(sink.writer())?;
    }

    Ok(())
}

/// A `norn repair --plan <flags> <trailing>` command string for the report's
/// apply-guidance lines.
fn repair_command(filter_flags: &[String], trailing: &[&str]) -> String {
    let mut parts: Vec<String> = vec!["norn".into(), "repair".into(), "--plan".into()];
    parts.extend(filter_flags.iter().cloned());
    parts.extend(trailing.iter().map(|s| s.to_string()));
    parts.join(" ")
}

/// User-facing prose for a stable skip-reason code. One point of evolution: a
/// new `SkipReason::code()` variant adds a matching arm here.
fn skip_reason_prose(code: &str) -> &'static str {
    match code {
        "missing-default" => "missing field has no configured deterministic default",
        "link-decision-needed" => "link repair requires an explicit path/link decision",
        "no-rule-matched" => "no configured deterministic repair rule matched",
        "alias-shadowed" => "alias shadowed by a doc stem cannot be repaired deterministically",
        "graph-diagnostic" => "graph diagnostic cannot be repaired deterministically",
        "ambiguous-target" => "ambiguous link target",
        "missing-hash" => "index missing hash for finding's path",
        "precondition-failed" => "rule precondition blocked producing a change",
        _ => "(unknown skip reason)",
    }
}
