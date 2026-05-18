use std::fs;

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use vault_core::GraphIndex;
use vault_standards::apply::{
    apply_file_changes, changes_by_path, validate_plan_for_apply, ApplyError,
};
use vault_standards::{Finding, RepairPlan};

#[allow(unused_imports)]
pub use vault_standards::apply::{
    RepairApplyPlanContext, RepairApplyReport, RepairApplyVerification,
};

pub fn apply_repair_plan(
    cwd: &Utf8PathBuf,
    index: &GraphIndex,
    plan: &RepairPlan,
    dry_run: bool,
) -> Result<RepairApplyReport> {
    validate_plan_for_apply(cwd, plan)?;

    let grouped = changes_by_path(plan)?;

    let mut report = RepairApplyReport::new(plan, dry_run);

    let current_hashes: std::collections::BTreeMap<Utf8PathBuf, String> = index
        .documents
        .iter()
        .map(|d| (d.path.clone(), d.hash.clone()))
        .collect();

    for (rel_path, changes) in &grouped {
        let current_hash = current_hashes.get(rel_path).ok_or_else(|| {
            anyhow::anyhow!(ApplyError::UnknownPath {
                path: rel_path.clone(),
            })
        })?;
        let plan_hash = &changes[0].document_hash;
        if current_hash != plan_hash {
            return Err(anyhow::anyhow!(ApplyError::StaleDocumentHash {
                path: rel_path.clone(),
                expected: plan_hash.clone(),
                actual: current_hash.clone(),
            }));
        }

        let absolute_path = cwd.join(rel_path);
        let original = fs::read_to_string(&absolute_path)
            .with_context(|| format!("read {absolute_path}"))?;
        let updated = apply_file_changes(&original, changes)?;

        if updated != original {
            report.changed_files.push(rel_path.clone());
            if !dry_run {
                fs::write(&absolute_path, updated)
                    .with_context(|| format!("write {absolute_path}"))?;
            }
        }
    }

    Ok(report)
}

pub fn with_verification(
    report: RepairApplyReport,
    findings: &[Finding],
) -> RepairApplyReport {
    report.with_verification(findings)
}
