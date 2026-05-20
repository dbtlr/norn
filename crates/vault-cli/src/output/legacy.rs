//! Pre-port output helpers. Commands that have not yet adopted `output::primitives`
//! import from here. When a command is ported, remove its imports from this module.
//! When this module has no remaining callers, delete it.

use std::collections::BTreeSet;
use std::io::{self, IsTerminal, Write};

use anyhow::{bail, Result};
use camino::Utf8PathBuf;
use serde::Serialize;
use serde_json::Value;
use vault_core::display;
use vault_core::{Link, VaultFile};
use vault_standards::{Finding, FindingBody, RepairPlan, Summary};

use crate::cli::OutputFormat;
use crate::filter::DocsSummaryReport;
use crate::link_repair::LinkRepairReport;
use crate::repair_apply::RepairApplyReport;

pub fn resolve_format(format: Option<OutputFormat>) -> OutputFormat {
    format.unwrap_or_else(|| {
        if io::stdout().is_terminal() {
            OutputFormat::Table
        } else {
            OutputFormat::Json
        }
    })
}

pub fn write_output<T: Serialize>(items: &[T], format: OutputFormat) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    match format {
        OutputFormat::Json => {
            write_json_line(&mut stdout, &serde_json::to_string_pretty(items)?)?;
        }
        OutputFormat::Jsonl => {
            for item in items {
                write_json_line(&mut stdout, &serde_json::to_string(item)?)?;
            }
        }
        OutputFormat::Table => write_generic_items_table(items)?,
        OutputFormat::Paths => write_generic_item_paths(items)?,
    }
    Ok(())
}

pub fn write_item_output<T: Serialize>(item: &T, format: OutputFormat) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    match format {
        OutputFormat::Json => {
            write_json_line(&mut stdout, &serde_json::to_string_pretty(item)?)?;
        }
        OutputFormat::Jsonl => {
            write_json_line(&mut stdout, &serde_json::to_string(item)?)?;
        }
        OutputFormat::Table => write_generic_item_table(item)?,
        OutputFormat::Paths => bail!("paths format is not supported for this command"),
    }
    Ok(())
}

pub fn write_document_summary(summary: &DocsSummaryReport, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json | OutputFormat::Jsonl => write_item_output(summary, format),
        OutputFormat::Paths => write_item_output(summary, OutputFormat::Json),
        OutputFormat::Table => {
            let rows = summary
                .counts
                .iter()
                .map(|(value, count)| vec![value.clone(), count.to_string()])
                .collect::<Vec<_>>();
            let title = vec![vec!["total".to_string(), summary.total.to_string()]];
            write_table(&["metric", "count"], &title)?;
            write_blank_line()?;
            write_table(&[summary.count_by.as_str(), "count"], &rows)
        }
    }
}

pub fn write_files(files: &[&VaultFile], format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json | OutputFormat::Jsonl => write_output(files, format),
        OutputFormat::Paths => write_paths(files.iter().map(|file| &file.path)),
        OutputFormat::Table => {
            let rows = files
                .iter()
                .map(|file| {
                    vec![
                        file.path.to_string(),
                        file.extension.clone().unwrap_or_default(),
                        file.hash.as_deref().map(short_hash).unwrap_or_default(),
                    ]
                })
                .collect::<Vec<_>>();
            write_table(&["path", "ext", "hash"], &rows)
        }
    }
}

pub fn write_links(links: &[&Link], format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json | OutputFormat::Jsonl => write_output(links, format),
        OutputFormat::Paths => {
            let paths = links
                .iter()
                .map(|link| link.source_path.clone())
                .collect::<BTreeSet<_>>();
            write_paths(paths.iter())
        }
        OutputFormat::Table => {
            let rows = links
                .iter()
                .map(|link| {
                    vec![
                        link.source_path.to_string(),
                        display::link_kind_str(&link.kind).to_string(),
                        link.target.clone(),
                        display::link_status_str(&link.status).to_string(),
                        link.resolved_path
                            .as_ref()
                            .map(ToString::to_string)
                            .unwrap_or_default(),
                    ]
                })
                .collect::<Vec<_>>();
            write_table(&["source", "kind", "target", "status", "resolved"], &rows)
        }
    }
}

pub fn write_findings(findings: &[Finding], format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json | OutputFormat::Jsonl => write_output(findings, format),
        OutputFormat::Paths => {
            let paths = findings
                .iter()
                .map(|finding| finding.path.clone())
                .collect::<BTreeSet<_>>();
            write_paths(paths.iter())
        }
        OutputFormat::Table => {
            let rows = findings
                .iter()
                .map(|finding| {
                    let (rule, field, target) = finding_context(finding);
                    vec![
                        finding.path.to_string(),
                        finding.code.clone(),
                        display::severity_str(&finding.severity).to_string(),
                        rule,
                        field,
                        target,
                    ]
                })
                .collect::<Vec<_>>();
            write_table(
                &["path", "code", "severity", "rule", "field", "target"],
                &rows,
            )
        }
    }
}

pub fn write_validate_summary(summary: &Summary, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json | OutputFormat::Jsonl => write_item_output(summary, format),
        OutputFormat::Paths => write_item_output(summary, OutputFormat::Json),
        OutputFormat::Table => {
            let totals = vec![vec!["findings".to_string(), summary.findings.to_string()]];
            write_table(&["metric", "count"], &totals)?;
            write_blank_line()?;
            write_count_rows("codes", &summary.codes)?;
            write_blank_line()?;
            write_count_rows("severities", &summary.severities)?;
            if !summary.rules.is_empty() {
                write_blank_line()?;
                write_count_rows("rules", &summary.rules)?;
            }
            if !summary.fields.is_empty() {
                write_blank_line()?;
                write_count_rows("fields", &summary.fields)?;
            }
            if !summary.path_prefixes.is_empty() {
                write_blank_line()?;
                write_count_rows("path_prefixes", &summary.path_prefixes)?;
            }
            Ok(())
        }
    }
}

pub fn write_repair_plan(plan: &RepairPlan, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json | OutputFormat::Jsonl => write_item_output(plan, format),
        OutputFormat::Paths => bail!("paths format is not supported for repair plans"),
        OutputFormat::Table => {
            let summary = vec![
                vec!["findings".to_string(), plan.summary.findings.to_string()],
                vec![
                    "planned_changes".to_string(),
                    plan.summary.planned_changes.to_string(),
                ],
                vec![
                    "skipped/unsupported".to_string(),
                    plan.summary.skipped.unsupported.to_string(),
                ],
                vec![
                    "skipped/ambiguous".to_string(),
                    plan.summary.skipped.ambiguous.to_string(),
                ],
                vec![
                    "skipped/missing_hash".to_string(),
                    plan.summary.skipped.missing_hash.to_string(),
                ],
                vec![
                    "skipped/precondition_failed".to_string(),
                    plan.summary.skipped.precondition_failed.to_string(),
                ],
                vec![
                    "skipped/total".to_string(),
                    plan.summary.skipped.total.to_string(),
                ],
            ];
            write_table(&["metric", "count"], &summary)?;
            if !plan.changes.is_empty() {
                write_blank_line()?;
                let rows = plan
                    .changes
                    .iter()
                    .map(|change| {
                        vec![
                            change.path.to_string(),
                            change.operation.clone(),
                            change.field.clone().unwrap_or_default(),
                            change
                                .expected_old_value
                                .as_ref()
                                .map(display_value)
                                .unwrap_or_default(),
                            change
                                .new_value
                                .as_ref()
                                .map(display_value)
                                .unwrap_or_default(),
                            change.repair_rule.clone(),
                        ]
                    })
                    .collect::<Vec<_>>();
                write_table(
                    &["path", "operation", "field", "old", "new", "repair_rule"],
                    &rows,
                )?;
            }
            if !plan.skipped_findings.is_empty() {
                write_blank_line()?;
                let rows = plan
                    .skipped_findings
                    .iter()
                    .map(|finding| {
                        vec![
                            finding.path.to_string(),
                            finding.code.clone(),
                            finding.field.clone().unwrap_or_default(),
                            finding.target.clone().unwrap_or_default(),
                            finding.reason.clone(),
                        ]
                    })
                    .collect::<Vec<_>>();
                write_table(&["path", "code", "field", "target", "reason"], &rows)?;
            }
            Ok(())
        }
    }
}

pub fn write_link_repair_report(report: &LinkRepairReport, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json | OutputFormat::Jsonl => write_item_output(report, format),
        OutputFormat::Paths => bail!("paths format is not supported for link repair reports"),
        OutputFormat::Table => {
            let summary = vec![
                vec![
                    "unresolved_links".to_string(),
                    report.summary.unresolved_links.to_string(),
                ],
                vec![
                    "ambiguous_links".to_string(),
                    report.summary.ambiguous_links.to_string(),
                ],
                vec![
                    "path_style_markdown_links".to_string(),
                    report.summary.path_style_markdown_links.to_string(),
                ],
                vec![
                    "duplicate_stem_risks".to_string(),
                    report.summary.duplicate_stem_risks.to_string(),
                ],
                vec![
                    "affected_files".to_string(),
                    report.summary.affected_files.to_string(),
                ],
            ];
            write_table(&["metric", "count"], &summary)?;

            let mut rows = Vec::new();
            for link in &report.unresolved_links {
                rows.push(link_repair_row("unresolved", link));
            }
            for link in &report.ambiguous_links {
                rows.push(link_repair_row("ambiguous", link));
            }
            for link in &report.path_style_markdown_links {
                rows.push(link_repair_row("path-style", link));
            }
            if !rows.is_empty() {
                write_blank_line()?;
                write_table(
                    &["category", "source", "target", "reason", "decision"],
                    &rows,
                )?;
            }
            if let Some(target_risk) = &report.target_risk {
                write_blank_line()?;
                let rows = vec![
                    vec![
                        "target_path".to_string(),
                        target_risk.target_path.to_string(),
                    ],
                    vec![
                        "incoming_link_count".to_string(),
                        target_risk.incoming_link_count.to_string(),
                    ],
                    vec![
                        "incoming_sources".to_string(),
                        target_risk
                            .incoming_links
                            .iter()
                            .map(|link| link.source_path.to_string())
                            .collect::<Vec<_>>()
                            .join(","),
                    ],
                ];
                write_table(&["target_risk", "value"], &rows)?;
            }
            Ok(())
        }
    }
}

pub fn write_repair_apply_report(report: &RepairApplyReport, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json | OutputFormat::Jsonl => write_item_output(report, format),
        OutputFormat::Paths => bail!("paths format is not supported for repair apply reports"),
        OutputFormat::Table => {
            let mut rows = vec![
                vec!["dry_run".to_string(), report.dry_run.to_string()],
                vec![
                    "applied_changes".to_string(),
                    report.applied_changes.to_string(),
                ],
                vec![
                    "changed_files".to_string(),
                    report
                        .changed_files
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(","),
                ],
                vec![
                    "skipped/unsupported".to_string(),
                    report.plan_context.skipped.unsupported.to_string(),
                ],
                vec![
                    "skipped/ambiguous".to_string(),
                    report.plan_context.skipped.ambiguous.to_string(),
                ],
                vec![
                    "skipped/missing_hash".to_string(),
                    report.plan_context.skipped.missing_hash.to_string(),
                ],
                vec![
                    "skipped/precondition_failed".to_string(),
                    report.plan_context.skipped.precondition_failed.to_string(),
                ],
                vec![
                    "skipped/total".to_string(),
                    report.plan_context.skipped.total.to_string(),
                ],
            ];
            if let Some(verification) = &report.verification {
                rows.push(vec![
                    "remaining_findings".to_string(),
                    verification.remaining_findings.to_string(),
                ]);
            }
            write_table(&["metric", "value"], &rows)
        }
    }
}

fn write_json_line(stdout: &mut impl Write, json: &str) -> Result<()> {
    stdout.write_all(json.as_bytes())?;
    stdout.write_all(b"\n")?;
    Ok(())
}

fn link_repair_row(category: &str, link: &crate::link_repair::LinkDecision) -> Vec<String> {
    vec![
        category.to_string(),
        link.source_path.to_string(),
        link.target.clone(),
        link.unresolved_reason.clone().unwrap_or_default(),
        link.decision.clone(),
    ]
}

fn write_paths<'a>(paths: impl IntoIterator<Item = &'a Utf8PathBuf>) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    for path in paths {
        writeln!(stdout, "{path}")?;
    }
    Ok(())
}

fn write_table(headers: &[&str], rows: &[Vec<String>]) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    let widths = column_widths(headers, rows);

    write_table_row(&mut stdout, headers.iter().copied(), &widths)?;
    let separators = widths
        .iter()
        .map(|width| "-".repeat(*width))
        .collect::<Vec<_>>();
    write_table_row(&mut stdout, separators.iter().map(String::as_str), &widths)?;
    for row in rows {
        write_table_row(&mut stdout, row.iter().map(String::as_str), &widths)?;
    }
    Ok(())
}

fn write_table_row<'a>(
    stdout: &mut impl Write,
    cells: impl IntoIterator<Item = &'a str>,
    widths: &[usize],
) -> Result<()> {
    let cells = cells.into_iter().collect::<Vec<_>>();
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            stdout.write_all(b"  ")?;
        }
        let cell = cells.get(index).copied().unwrap_or("");
        write!(stdout, "{cell:<width$}")?;
    }
    stdout.write_all(b"\n")?;
    Ok(())
}

fn write_count_rows(label: &str, counts: &std::collections::BTreeMap<String, usize>) -> Result<()> {
    let rows = counts
        .iter()
        .map(|(key, count)| vec![key.clone(), count.to_string()])
        .collect::<Vec<_>>();
    write_table(&[label, "count"], &rows)
}

fn write_blank_line() -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    stdout.write_all(b"\n")?;
    Ok(())
}

fn write_generic_items_table<T: Serialize>(items: &[T]) -> Result<()> {
    let values = items
        .iter()
        .map(serde_json::to_value)
        .collect::<serde_json::Result<Vec<_>>>()?;
    let headers = table_headers(&values);
    let header_refs = headers.iter().map(String::as_str).collect::<Vec<_>>();
    let rows = values
        .iter()
        .map(|value| table_row_for(value, &headers))
        .collect::<Vec<_>>();
    write_table(&header_refs, &rows)
}

fn write_generic_item_table<T: Serialize>(item: &T) -> Result<()> {
    let value = serde_json::to_value(item)?;
    match value {
        Value::Object(object) => {
            let rows = object
                .iter()
                .map(|(key, value)| vec![key.clone(), display_value(value)])
                .collect::<Vec<_>>();
            write_table(&["field", "value"], &rows)
        }
        value => {
            let rows = vec![vec![display_value(&value)]];
            write_table(&["value"], &rows)
        }
    }
}

fn write_generic_item_paths<T: Serialize>(items: &[T]) -> Result<()> {
    let values = items
        .iter()
        .map(serde_json::to_value)
        .collect::<serde_json::Result<Vec<_>>>()?;
    let paths = values
        .iter()
        .map(|value| {
            value
                .get("path")
                .and_then(Value::as_str)
                .map(Utf8PathBuf::from)
                .ok_or_else(|| anyhow::anyhow!("paths format is not supported for this command"))
        })
        .collect::<Result<Vec<_>>>()?;
    write_paths(paths.iter())
}

fn table_headers(values: &[Value]) -> Vec<String> {
    let mut headers = BTreeSet::new();
    for value in values {
        if let Value::Object(object) = value {
            headers.extend(object.keys().cloned());
        }
    }
    headers.into_iter().collect()
}

fn table_row_for(value: &Value, headers: &[String]) -> Vec<String> {
    headers
        .iter()
        .map(|header| value.get(header).map(display_value).unwrap_or_default())
        .collect()
}

fn column_widths(headers: &[&str], rows: &[Vec<String>]) -> Vec<usize> {
    headers
        .iter()
        .enumerate()
        .map(|(index, header)| {
            rows.iter()
                .filter_map(|row| row.get(index))
                .map(|cell| cell.chars().count())
                .max()
                .unwrap_or(0)
                .max(header.chars().count())
        })
        .collect()
}

fn display_value(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Null => String::new(),
        Value::Array(_) | Value::Object(_) => serde_json::to_string(value).unwrap_or_default(),
    }
}

fn short_hash(hash: &str) -> String {
    hash.chars().take(12).collect()
}

fn finding_context(finding: &Finding) -> (String, String, String) {
    match &finding.body {
        FindingBody::GraphDiagnostic { .. } => (String::new(), String::new(), String::new()),
        FindingBody::LinkIssue { link } => (String::new(), String::new(), link.target.clone()),
        FindingBody::RequiredFrontmatterMissing { rule, field }
        | FindingBody::ForbiddenField { rule, field, .. }
        | FindingBody::InvalidFieldType { rule, field, .. }
        | FindingBody::DisallowedValue { rule, field, .. } => (
            rule.clone().unwrap_or_default(),
            field.clone(),
            String::new(),
        ),
        FindingBody::DocumentMisrouted { rule, .. } => (
            rule.clone().unwrap_or_default(),
            String::new(),
            String::new(),
        ),
    }
}

pub fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
    })
}
