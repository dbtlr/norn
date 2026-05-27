//! Repair-apply rendering (Report / Paths formats).
//!
//! `render_report` composes the TTY summary (headline, severity tally,
//! by-operation tally, optional warnings sub-block, footer) from
//! `output::primitives`. `write_paths` emits the sorted dedup of
//! `changed_files` for the `paths` format.

/// Format a `PlanWarning` into a human-readable string for TTY rendering.
///
/// `PlanWarning` does not derive `Display`, so each variant is matched explicitly.
fn format_plan_warning(w: &crate::standards::PlanWarning) -> String {
    use crate::standards::PlanWarning::*;
    match w {
        StemCollisionAfterMove {
            new_stem,
            new_path: _,
            collides_with,
        } => {
            let others = collides_with
                .iter()
                .map(|p| p.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!("stem collision: '{new_stem}' already exists at {others}")
        }
    }
}

/// Map a `PlannedChange.operation` snake_case identifier to its kebab-case TTY form.
///
/// Mirrors the `SkipReason` precedent: snake in JSON contract, kebab in TTY rendering.
/// Unknown operations fall through unchanged — preserves forward-compat if future
/// orchestrator additions land before this helper is updated.
pub(crate) fn operation_code(op: &str) -> &str {
    match op {
        "set_frontmatter" => "set-frontmatter",
        "add_frontmatter" => "add-frontmatter",
        "remove_frontmatter" => "remove-frontmatter",
        "rewrite_link" => "rewrite-link",
        "move_document" => "move-document",
        other => other,
    }
}

use std::collections::BTreeMap;
use std::io::{self, Write};

use crate::standards::apply::RepairApplyReport;
use camino::Utf8PathBuf;

use crate::output::palette;
use crate::output::primitives::tally_group;

pub(crate) fn write_paths(report: &RepairApplyReport, out: &mut dyn Write) -> io::Result<()> {
    let mut paths: Vec<&Utf8PathBuf> = report.changed_files.iter().collect();
    paths.sort();
    paths.dedup();
    for p in paths {
        writeln!(out, "{p}")?;
    }
    Ok(())
}

/// Where the plan came from. Drives the headline string: file paths render verbatim,
/// stdin renders as the literal "from stdin".
pub(crate) enum PlanSource {
    File(Utf8PathBuf),
    Stdin,
}

impl PlanSource {
    fn headline_target(&self) -> String {
        match self {
            PlanSource::File(path) => path.to_string(),
            PlanSource::Stdin => "from stdin".to_string(),
        }
    }
}

/// Render the TTY summary for a `RepairApplyReport`. Composes existing primitives
/// plus inline severity tally and footer logic. Empty-plan short-circuit handled here.
pub(crate) fn render_report(
    report: &RepairApplyReport,
    plan: &crate::standards::RepairPlan,
    source: PlanSource,
    out: &mut dyn Write,
) -> io::Result<()> {
    let total_changes = report.applied_changes;

    if total_changes == 0 {
        writeln!(out, "0 changes from plan · nothing to do")?;
        return Ok(());
    }

    let verb = if report.dry_run {
        "preview"
    } else {
        "applying"
    };
    let target = source.headline_target();
    let suffix = if report.dry_run {
        " · no files written"
    } else {
        ""
    };
    writeln!(
        out,
        "{verb} {target} · {total_changes} changes from plan{suffix}"
    )?;
    writeln!(out)?; // blank line before the severity tally

    let applied_verb = if report.dry_run {
        "would apply"
    } else {
        "applied"
    };
    let warn_count = report.warnings.len();
    let max = total_changes.max(warn_count);
    let w = max.to_string().len();
    writeln!(out, "  ✓ {total_changes:>w$} changes {applied_verb}")?;
    if warn_count > 0 {
        let label = if warn_count == 1 {
            "warning"
        } else {
            "warnings"
        };
        writeln!(out, "  ⚠ {warn_count:>w$} {label}")?;
    }
    writeln!(out)?; // blank line before by-operation

    let mut by_op: BTreeMap<&str, usize> = BTreeMap::new();
    for change in &plan.changes {
        *by_op.entry(operation_code(&change.operation)).or_insert(0) += 1;
    }
    if !by_op.is_empty() {
        let p = palette::resolve(crate::cli::ColorWhen::Auto);
        // Sort by count descending, then alphabetically by code.
        let mut rows: Vec<(&str, usize)> = by_op.into_iter().collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        tally_group(out, &p, "by operation", &rows, 80)?;
        writeln!(out)?; // blank before footer (Task 10)
    }

    // Warnings sub-block (Task 9).
    if !report.warnings.is_empty() {
        writeln!(out, "  warnings")?;
        for w in &report.warnings {
            let op = plan
                .changes
                .iter()
                .find(|c| c.path == w.path)
                .map(|c| operation_code(&c.operation))
                .unwrap_or("warning");
            writeln!(out, "    {op}  {}", w.path)?;
            writeln!(out, "      {}", format_plan_warning(&w.warning))?;
            writeln!(out)?;
        }
    }

    // Footer (Task 10): totals + optional warnings count + optional verify + action hint.
    let files_count = report.changed_files.len();
    let warn_count = report.warnings.len();

    let mut parts: Vec<String> = Vec::new();
    if report.dry_run {
        parts.push(format!("{total_changes} changes preflight"));
        parts.push(format!("{files_count} files would change"));
    } else {
        parts.push(format!("{total_changes} of {total_changes} applied"));
        parts.push(format!("{files_count} files changed"));
    }

    if warn_count > 0 {
        let label = if warn_count == 1 {
            "warning"
        } else {
            "warnings"
        };
        parts.push(format!("{warn_count} {label}"));
    }

    if let Some(v) = &report.verification {
        parts.push(format!(
            "verify: {} findings remaining",
            v.remaining_findings
        ));
    }

    let action_hint = if report.dry_run {
        "run without --dry-run to apply"
    } else {
        "run `vault validate` to verify"
    };

    // When verify is present, it replaces the action hint (already inline in the parts).
    if report.verification.is_none() {
        parts.push(action_hint.to_string());
    }

    writeln!(out, "{}", parts.join(" · "))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::standards::apply::RepairApplyReport;
    use crate::standards::{RepairPlan, RepairPlanFilters, RepairPlanSummary, SkippedSummary};
    use camino::Utf8PathBuf;

    #[test]
    fn render_report_with_empty_plan_emits_nothing_to_do_line() {
        let plan = RepairPlan {
            schema_version: 6,
            vault_root: Utf8PathBuf::from("/tmp"),
            source_filters: RepairPlanFilters::default(),
            summary: RepairPlanSummary {
                findings: 0,
                planned_changes: 0,
                skipped: SkippedSummary::default(),
            },
            changes: vec![],
            skipped_findings: vec![],
            footnotes: vec![],
        };
        let report = RepairApplyReport::new(&plan, false);
        let mut buf: Vec<u8> = Vec::new();
        let source = PlanSource::File(Utf8PathBuf::from("plan.json"));
        render_report(&report, &plan, source, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("0 changes from plan"),
            "expected empty-plan line, got: {out}"
        );
        assert!(
            out.contains("nothing to do"),
            "expected nothing-to-do hint, got: {out}"
        );
    }

    fn fixture_report(changes: usize, dry_run: bool) -> (RepairPlan, RepairApplyReport) {
        use crate::standards::{
            PlannedChange, RepairPlanFilters, RepairPlanSummary, SkippedSummary,
        };
        let plan = RepairPlan {
            schema_version: 6,
            vault_root: Utf8PathBuf::from("/tmp"),
            source_filters: RepairPlanFilters::default(),
            summary: RepairPlanSummary {
                findings: changes,
                planned_changes: changes,
                skipped: SkippedSummary::default(),
            },
            changes: (0..changes)
                .map(|i| PlannedChange {
                    change_id: format!("id{i}"),
                    path: Utf8PathBuf::from(format!("notes/file{i}.md")),
                    document_hash: String::new(),
                    finding_code: "LC001".to_string(),
                    finding_rule: None,
                    repair_rule: "rewrite_link".to_string(),
                    operation: "rewrite_link".to_string(),
                    field: None,
                    expected_old_value: None,
                    new_value: None,
                    destination: None,
                    link_risk: None,
                    warnings: vec![],
                    force: false,
                })
                .collect(),
            skipped_findings: vec![],
            footnotes: vec![],
        };
        let report = RepairApplyReport::new(&plan, dry_run);
        (plan, report)
    }

    #[test]
    fn headline_for_file_source_real_apply() {
        let (plan, report) = fixture_report(134, false);
        let mut buf: Vec<u8> = Vec::new();
        render_report(
            &report,
            &plan,
            PlanSource::File(Utf8PathBuf::from("repair.json")),
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        let first = out.lines().next().unwrap();
        assert_eq!(first, "applying repair.json · 134 changes from plan");
    }

    #[test]
    fn headline_for_stdin_real_apply() {
        let (plan, report) = fixture_report(134, false);
        let mut buf: Vec<u8> = Vec::new();
        render_report(&report, &plan, PlanSource::Stdin, &mut buf).unwrap();
        let first = String::from_utf8(buf)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string();
        assert_eq!(first, "applying from stdin · 134 changes from plan");
    }

    #[test]
    fn headline_for_dry_run_swaps_verbs_and_adds_no_files_written() {
        let (plan, report) = fixture_report(134, true);
        let mut buf: Vec<u8> = Vec::new();
        render_report(
            &report,
            &plan,
            PlanSource::File(Utf8PathBuf::from("repair.json")),
            &mut buf,
        )
        .unwrap();
        let first = String::from_utf8(buf)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string();
        assert_eq!(
            first,
            "preview repair.json · 134 changes from plan · no files written"
        );
    }

    #[test]
    fn severity_tally_success_path_shows_only_applied_row() {
        let (plan, report) = fixture_report(134, false);
        let mut buf: Vec<u8> = Vec::new();
        render_report(
            &report,
            &plan,
            PlanSource::File(Utf8PathBuf::from("repair.json")),
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("✓ 134 changes applied"),
            "expected ✓ row, got:\n{out}"
        );
        assert!(
            !out.contains("⚠"),
            "no warnings should be shown, got:\n{out}"
        );
    }

    #[test]
    fn severity_tally_dry_run_says_would_apply() {
        let (plan, report) = fixture_report(134, true);
        let mut buf: Vec<u8> = Vec::new();
        render_report(
            &report,
            &plan,
            PlanSource::File(Utf8PathBuf::from("repair.json")),
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("✓ 134 changes would apply"),
            "expected 'would apply' verb, got:\n{out}"
        );
    }

    #[test]
    fn severity_tally_with_warnings_shows_both_rows() {
        use crate::standards::apply::RepairApplyWarning;
        use crate::standards::PlanWarning;
        let (plan, mut report) = fixture_report(134, false);
        report.warnings = vec![
            RepairApplyWarning {
                path: Utf8PathBuf::from("inbox/a.md"),
                warning: PlanWarning::StemCollisionAfterMove {
                    new_stem: "a".to_string(),
                    new_path: Utf8PathBuf::from("inbox/a.md"),
                    collides_with: vec![Utf8PathBuf::from("archive/a.md")],
                },
            },
            RepairApplyWarning {
                path: Utf8PathBuf::from("inbox/b.md"),
                warning: PlanWarning::StemCollisionAfterMove {
                    new_stem: "b".to_string(),
                    new_path: Utf8PathBuf::from("inbox/b.md"),
                    collides_with: vec![Utf8PathBuf::from("archive/b.md")],
                },
            },
        ];
        let mut buf: Vec<u8> = Vec::new();
        render_report(
            &report,
            &plan,
            PlanSource::File(Utf8PathBuf::from("repair.json")),
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("✓ 134 changes applied"),
            "expected ✓ row, got:\n{out}"
        );
        assert!(
            out.contains("⚠   2 warnings"),
            "expected ⚠ row with right-aligned count, got:\n{out}"
        );
    }

    #[test]
    fn operation_code_maps_all_known_snake_to_kebab() {
        assert_eq!(operation_code("set_frontmatter"), "set-frontmatter");
        assert_eq!(operation_code("add_frontmatter"), "add-frontmatter");
        assert_eq!(operation_code("remove_frontmatter"), "remove-frontmatter");
        assert_eq!(operation_code("rewrite_link"), "rewrite-link");
        assert_eq!(operation_code("move_document"), "move-document");
    }

    #[test]
    fn operation_code_passes_through_unknown_unchanged() {
        assert_eq!(operation_code("future_operation"), "future_operation");
        assert_eq!(operation_code(""), "");
    }

    #[test]
    fn by_operation_groups_changes_with_kebab_codes() {
        let plan = make_plan_with_mixed_operations();
        let report = RepairApplyReport::new(&plan, false);
        let mut buf: Vec<u8> = Vec::new();
        render_report(
            &report,
            &plan,
            PlanSource::File(Utf8PathBuf::from("repair.json")),
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("by operation"),
            "expected section header, got:\n{out}"
        );
        assert!(
            out.contains("rewrite-link"),
            "expected kebab op, got:\n{out}"
        );
        assert!(
            out.contains("add-frontmatter"),
            "expected kebab op, got:\n{out}"
        );
        assert!(
            out.contains("move-document"),
            "expected kebab op, got:\n{out}"
        );
        // rewrite-link → 2, others → 1 each
        let rewrite_line = out.lines().find(|l| l.contains("rewrite-link")).unwrap();
        assert!(
            rewrite_line.contains("2"),
            "rewrite-link should have count 2, got: {rewrite_line}"
        );
    }

    #[test]
    fn by_operation_section_suppressed_when_zero_changes() {
        let plan = empty_plan();
        let report = RepairApplyReport::new(&plan, false);
        let mut buf: Vec<u8> = Vec::new();
        render_report(
            &report,
            &plan,
            PlanSource::File(Utf8PathBuf::from("plan.json")),
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            !out.contains("by operation"),
            "no by-operation section for empty plan"
        );
    }

    // Helpers for by-operation task fixtures
    fn make_plan_with_mixed_operations() -> crate::standards::RepairPlan {
        use crate::standards::{
            PlannedChange, RepairPlan, RepairPlanFilters, RepairPlanSummary, SkippedSummary,
        };
        let make_change = |path: &str, op: &str| PlannedChange {
            change_id: format!("c-{}-{}", path, op),
            path: Utf8PathBuf::from(path),
            document_hash: String::new(),
            finding_code: String::new(),
            finding_rule: None,
            repair_rule: String::new(),
            operation: op.to_string(),
            field: None,
            expected_old_value: None,
            new_value: None,
            destination: None,
            link_risk: None,
            warnings: vec![],
            force: false,
        };
        RepairPlan {
            schema_version: 6,
            vault_root: Utf8PathBuf::from("/tmp"),
            source_filters: RepairPlanFilters::default(),
            summary: RepairPlanSummary {
                findings: 4,
                planned_changes: 4,
                skipped: SkippedSummary::default(),
            },
            changes: vec![
                make_change("a.md", "rewrite_link"),
                make_change("b.md", "rewrite_link"),
                make_change("c.md", "add_frontmatter"),
                make_change("d.md", "move_document"),
            ],
            skipped_findings: vec![],
            footnotes: vec![],
        }
    }

    fn empty_plan() -> crate::standards::RepairPlan {
        use crate::standards::{RepairPlan, RepairPlanFilters, RepairPlanSummary, SkippedSummary};
        RepairPlan {
            schema_version: 6,
            vault_root: Utf8PathBuf::from("/tmp"),
            source_filters: RepairPlanFilters::default(),
            summary: RepairPlanSummary {
                findings: 0,
                planned_changes: 0,
                skipped: SkippedSummary::default(),
            },
            changes: vec![],
            skipped_findings: vec![],
            footnotes: vec![],
        }
    }

    #[test]
    fn footer_real_apply_shows_applied_count_and_files_changed() {
        let (plan, mut report) = fixture_report(134, false);
        report.changed_files = (0..75)
            .map(|i| Utf8PathBuf::from(format!("f{i}.md")))
            .collect();
        let mut buf: Vec<u8> = Vec::new();
        render_report(
            &report,
            &plan,
            PlanSource::File(Utf8PathBuf::from("repair.json")),
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        let last = out.lines().last().unwrap();
        assert_eq!(
            last,
            "134 of 134 applied · 75 files changed · run `vault validate` to verify"
        );
    }

    #[test]
    fn footer_dry_run_shows_preflight_and_would_change() {
        let (plan, mut report) = fixture_report(134, true);
        report.changed_files = (0..75)
            .map(|i| Utf8PathBuf::from(format!("f{i}.md")))
            .collect();
        let mut buf: Vec<u8> = Vec::new();
        render_report(
            &report,
            &plan,
            PlanSource::File(Utf8PathBuf::from("plan.json")),
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        let last = out.lines().last().unwrap();
        assert_eq!(
            last,
            "134 changes preflight · 75 files would change · run without --dry-run to apply"
        );
    }

    #[test]
    fn footer_includes_warnings_count_when_present() {
        use crate::standards::apply::RepairApplyWarning;
        use crate::standards::PlanWarning;
        let (plan, mut report) = fixture_report(1, false);
        report.changed_files = vec![Utf8PathBuf::from("a.md")];
        report.warnings = vec![RepairApplyWarning {
            path: Utf8PathBuf::from("a.md"),
            warning: PlanWarning::StemCollisionAfterMove {
                new_stem: "a".into(),
                new_path: Utf8PathBuf::from("a.md"),
                collides_with: vec![Utf8PathBuf::from("b.md")],
            },
        }];
        let mut buf: Vec<u8> = Vec::new();
        render_report(
            &report,
            &plan,
            PlanSource::File(Utf8PathBuf::from("plan.json")),
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        let last = out.lines().last().unwrap();
        assert!(
            last.contains("1 warning"),
            "footer must mention warnings count, got: {last}",
        );
    }

    #[test]
    fn footer_includes_verify_inline_when_verification_present() {
        use crate::standards::apply::RepairApplyVerification;
        let (plan, mut report) = fixture_report(1, false);
        report.changed_files = vec![Utf8PathBuf::from("a.md")];
        let summary = crate::standards::summarize(&[]);
        report.verification = Some(RepairApplyVerification {
            remaining_findings: 23,
            summary,
        });
        let mut buf: Vec<u8> = Vec::new();
        render_report(
            &report,
            &plan,
            PlanSource::File(Utf8PathBuf::from("plan.json")),
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        let last = out.lines().last().unwrap();
        assert!(
            last.contains("verify: 23 findings remaining"),
            "footer must contain verify, got: {last}"
        );
    }

    #[test]
    fn write_paths_emits_sorted_dedup_one_per_line() {
        let (plan, mut report) = fixture_report(0, false);
        let _ = plan; // unused in this test
        report.changed_files = vec![
            Utf8PathBuf::from("z.md"),
            Utf8PathBuf::from("a.md"),
            Utf8PathBuf::from("m.md"),
            Utf8PathBuf::from("a.md"), // duplicate
        ];
        let mut buf: Vec<u8> = Vec::new();
        write_paths(&report, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out, "a.md\nm.md\nz.md\n");
    }

    #[test]
    fn write_paths_empty_on_zero_changed_files() {
        let (_, report) = fixture_report(0, false);
        let mut buf: Vec<u8> = Vec::new();
        write_paths(&report, &mut buf).unwrap();
        assert_eq!(buf, b"", "empty changed_files → zero bytes");
    }

    #[test]
    fn warnings_sub_block_renders_each_warning_with_kebab_op() {
        use crate::standards::apply::RepairApplyWarning;
        use crate::standards::PlanWarning;
        use crate::standards::PlannedChange;
        let mut plan = empty_plan();
        plan.changes.push(PlannedChange {
            change_id: "c1".into(),
            path: Utf8PathBuf::from("inbox/a.md"),
            document_hash: String::new(),
            finding_code: String::new(),
            finding_rule: None,
            repair_rule: String::new(),
            operation: "move_document".into(),
            field: None,
            expected_old_value: None,
            new_value: None,
            destination: None,
            link_risk: None,
            warnings: vec![],
            force: false,
        });
        plan.summary.findings = 1;
        plan.summary.planned_changes = 1;
        let mut report = RepairApplyReport::new(&plan, false);
        report.warnings = vec![RepairApplyWarning {
            path: Utf8PathBuf::from("inbox/a.md"),
            warning: PlanWarning::StemCollisionAfterMove {
                new_stem: "a".to_string(),
                new_path: Utf8PathBuf::from("inbox/a.md"),
                collides_with: vec![Utf8PathBuf::from("archive/a.md")],
            },
        }];
        let mut buf: Vec<u8> = Vec::new();
        render_report(
            &report,
            &plan,
            PlanSource::File(Utf8PathBuf::from("repair.json")),
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("warnings"),
            "expected warnings section, got:\n{out}"
        );
        // Header line: "<kebab-op>  <path>"
        assert!(
            out.contains("move-document  inbox/a.md"),
            "expected operation+path header, got:\n{out}"
        );
    }
}
