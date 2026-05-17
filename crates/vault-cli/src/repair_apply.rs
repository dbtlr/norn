use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use anyhow::{bail, Result};
use camino::Utf8PathBuf;
use serde::Serialize;
use serde_json::Value;
use vault_core::{Diagnostic, GraphIndex};
use vault_frontmatter::extract_frontmatter;
use vault_standards::{summarize, RepairPlan, Summary};

#[derive(Debug, Serialize)]
pub struct RepairApplyReport {
    pub schema_version: u32,
    pub dry_run: bool,
    pub changed_files: Vec<Utf8PathBuf>,
    pub applied_changes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<RepairApplyVerification>,
}

#[derive(Debug, Serialize)]
pub struct RepairApplyVerification {
    pub remaining_findings: usize,
    pub summary: Summary,
}

pub fn apply_repair_plan(
    cwd: &Utf8PathBuf,
    index: &GraphIndex,
    plan: &RepairPlan,
    dry_run: bool,
) -> Result<RepairApplyReport> {
    validate_plan_for_apply(cwd, plan)?;
    let current_hashes = index
        .documents
        .iter()
        .map(|document| (document.path.clone(), document.hash.clone()))
        .collect::<BTreeMap<_, _>>();
    let changes_by_path = changes_by_path(plan)?;
    let mut changed_files = Vec::new();

    for (path, changes) in &changes_by_path {
        let current_hash = current_hashes.get(path).ok_or_else(|| {
            anyhow::anyhow!("plan targets a document that is not indexed: {path}")
        })?;
        let plan_hash = &changes[0].document_hash;
        if current_hash != plan_hash {
            bail!("stale repair plan for {path}: expected hash {plan_hash}, found {current_hash}");
        }

        let absolute_path = cwd.join(path);
        let content = fs::read_to_string(&absolute_path)
            .map_err(|error| anyhow::anyhow!("failed to read {absolute_path}: {error}"))?;
        let updated = apply_file_changes(&content, path, changes)?;
        if updated != content {
            changed_files.push(path.clone());
            if !dry_run {
                fs::write(&absolute_path, updated)
                    .map_err(|error| anyhow::anyhow!("failed to write {absolute_path}: {error}"))?;
            }
        }
    }

    Ok(RepairApplyReport {
        schema_version: plan.schema_version,
        dry_run,
        changed_files,
        applied_changes: plan.changes.len(),
        verification: None,
    })
}

pub fn with_verification(
    mut report: RepairApplyReport,
    findings: &[vault_standards::Finding],
) -> RepairApplyReport {
    let summary = summarize(findings);
    report.verification = Some(RepairApplyVerification {
        remaining_findings: summary.findings,
        summary,
    });
    report
}

fn validate_plan_for_apply(cwd: &Utf8PathBuf, plan: &RepairPlan) -> Result<()> {
    if plan.schema_version != 1 {
        bail!(
            "unsupported repair plan schema version: {}",
            plan.schema_version
        );
    }
    if &plan.vault_root != cwd {
        bail!(
            "repair plan vault root does not match effective cwd: plan={}, cwd={}",
            plan.vault_root,
            cwd
        );
    }
    if !plan.unsupported_findings.is_empty() {
        bail!("repair plan contains unsupported findings; refusing to apply");
    }
    if !plan.manual_decisions.is_empty() {
        bail!("repair plan contains manual decisions; refusing to apply");
    }
    Ok(())
}

fn changes_by_path(
    plan: &RepairPlan,
) -> Result<BTreeMap<Utf8PathBuf, Vec<&vault_standards::PlannedChange>>> {
    let mut changes_by_path: BTreeMap<Utf8PathBuf, Vec<&vault_standards::PlannedChange>> =
        BTreeMap::new();
    let mut seen_fields = BTreeSet::new();

    for change in &plan.changes {
        if !matches!(
            change.operation.as_str(),
            "set_frontmatter" | "remove_frontmatter"
        ) {
            bail!("unsupported repair operation: {}", change.operation);
        }
        let key = (change.path.clone(), change.field.clone());
        if !seen_fields.insert(key) {
            bail!(
                "repair plan contains conflicting changes for {} field {}",
                change.path,
                change.field
            );
        }
        changes_by_path
            .entry(change.path.clone())
            .or_default()
            .push(change);
    }

    for changes in changes_by_path.values() {
        let hash = &changes[0].document_hash;
        if changes.iter().any(|change| &change.document_hash != hash) {
            bail!("repair plan contains conflicting document hash preconditions");
        }
    }

    Ok(changes_by_path)
}

fn apply_file_changes(
    content: &str,
    path: &Utf8PathBuf,
    changes: &[&vault_standards::PlannedChange],
) -> Result<String> {
    let mut diagnostics = Vec::<Diagnostic>::new();
    let (frontmatter, frontmatter_range, _, _) = extract_frontmatter(content, &mut diagnostics);
    let Some(frontmatter_range) = frontmatter_range else {
        bail!("cannot apply frontmatter repairs to document without frontmatter: {path}");
    };
    if !diagnostics.is_empty() {
        bail!("cannot apply frontmatter repairs to document with invalid frontmatter: {path}");
    }
    let Some(frontmatter) = frontmatter else {
        bail!("cannot apply frontmatter repairs to document without parsed frontmatter: {path}");
    };
    let Some(current_object) = frontmatter.as_object() else {
        bail!("cannot apply frontmatter repairs to non-mapping frontmatter: {path}");
    };

    let yaml = &content[frontmatter_range.clone()];
    let mut yaml_value = serde_yaml::from_str::<serde_yaml::Value>(yaml)
        .map_err(|error| anyhow::anyhow!("failed to parse frontmatter for {path}: {error}"))?;
    let Some(mapping) = yaml_value.as_mapping_mut() else {
        bail!("cannot apply frontmatter repairs to non-mapping frontmatter: {path}");
    };

    for change in changes {
        let current_value = current_object.get(&change.field);
        match (&change.expected_old_value, current_value) {
            (Some(expected), Some(current)) if expected == current => {}
            (None, None) => {}
            (None, Some(Value::Null)) => {}
            (Some(expected), Some(current)) => bail!(
                "stale repair plan for {path} field {}: expected {}, found {}",
                change.field,
                expected,
                current
            ),
            (Some(expected), None) => bail!(
                "stale repair plan for {path} field {}: expected {}, found missing",
                change.field,
                expected
            ),
            (None, Some(current)) => bail!(
                "stale repair plan for {path} field {}: expected missing, found {}",
                change.field,
                current
            ),
        }

        let key = serde_yaml::Value::String(change.field.clone());
        match change.operation.as_str() {
            "set_frontmatter" => {
                let Some(new_value) = &change.new_value else {
                    bail!("set_frontmatter change missing new_value for {path}");
                };
                mapping.insert(key, serde_yaml::to_value(new_value)?);
            }
            "remove_frontmatter" => {
                mapping.remove(&key);
            }
            operation => bail!("unsupported repair operation: {operation}"),
        }
    }

    let mut new_yaml = serde_yaml::to_string(&yaml_value)?;
    if !new_yaml.ends_with('\n') {
        new_yaml.push('\n');
    }

    let mut updated = String::new();
    updated.push_str(&content[..frontmatter_range.start]);
    updated.push_str(&new_yaml);
    updated.push_str(&content[frontmatter_range.end..]);
    Ok(updated)
}
