use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use camino::Utf8PathBuf;
use serde::Serialize;
use vault_core::display;
use vault_core::{GraphIndex, Link, LinkKind, LinkStatus, UnresolvedReason};

use crate::target::{backlinks, resolve_backlink_target_path};

#[derive(Debug, Serialize)]
pub struct LinkRepairReport {
    pub schema_version: u32,
    pub summary: LinkRepairSummary,
    pub unresolved_links: Vec<LinkDecision>,
    pub ambiguous_links: Vec<LinkDecision>,
    pub path_style_markdown_links: Vec<LinkDecision>,
    pub duplicate_stem_risks: Vec<DuplicateStemRisk>,
    pub affected_files: Vec<Utf8PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_risk: Option<TargetPathRisk>,
}

#[derive(Debug, Serialize)]
pub struct LinkRepairSummary {
    pub unresolved_links: usize,
    pub ambiguous_links: usize,
    pub path_style_markdown_links: usize,
    pub duplicate_stem_risks: usize,
    pub affected_files: usize,
}

#[derive(Debug, Serialize)]
pub struct LinkDecision {
    pub source_path: Utf8PathBuf,
    pub raw: String,
    pub kind: String,
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unresolved_reason: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<Utf8PathBuf>,
    pub decision: String,
}

#[derive(Debug, Serialize)]
pub struct DuplicateStemRisk {
    pub stem: String,
    pub paths: Vec<Utf8PathBuf>,
    pub decision: String,
}

#[derive(Debug, Serialize)]
pub struct TargetPathRisk {
    pub target_path: Utf8PathBuf,
    pub incoming_link_count: usize,
    pub incoming_links: Vec<LinkDecision>,
    pub delete_risk: String,
    pub move_risk: String,
}

pub fn plan_link_repairs(index: &GraphIndex, target: Option<&str>) -> Result<LinkRepairReport> {
    let all_links = index
        .documents
        .iter()
        .flat_map(|document| document.links.iter())
        .collect::<Vec<_>>();
    let unresolved_links = all_links
        .iter()
        .copied()
        .filter(|link| link.status == LinkStatus::Unresolved)
        .map(link_decision)
        .collect::<Vec<_>>();
    let ambiguous_links = all_links
        .iter()
        .copied()
        .filter(|link| link.status == LinkStatus::Ambiguous)
        .map(link_decision)
        .collect::<Vec<_>>();
    let path_style_markdown_links = all_links
        .iter()
        .copied()
        .filter(|link| link.kind == LinkKind::Markdown && !link.target.starts_with("http"))
        .map(link_decision)
        .collect::<Vec<_>>();
    let duplicate_stem_risks = duplicate_stem_risks(index);
    let target_risk = target
        .map(|target| target_path_risk(index, target))
        .transpose()?;
    let affected_files = affected_files(
        &unresolved_links,
        &ambiguous_links,
        &path_style_markdown_links,
        target_risk.as_ref(),
    );

    Ok(LinkRepairReport {
        schema_version: 1,
        summary: LinkRepairSummary {
            unresolved_links: unresolved_links.len(),
            ambiguous_links: ambiguous_links.len(),
            path_style_markdown_links: path_style_markdown_links.len(),
            duplicate_stem_risks: duplicate_stem_risks.len(),
            affected_files: affected_files.len(),
        },
        unresolved_links,
        ambiguous_links,
        path_style_markdown_links,
        duplicate_stem_risks,
        affected_files,
        target_risk,
    })
}

fn target_path_risk(index: &GraphIndex, target: &str) -> Result<TargetPathRisk> {
    let target_path = resolve_backlink_target_path(index, target)?;
    let incoming_links = backlinks(index, &target_path)
        .into_iter()
        .map(link_decision)
        .collect::<Vec<_>>();
    let incoming_link_count = incoming_links.len();
    let delete_risk = if incoming_link_count == 0 {
        "no indexed incoming links; deletion may still affect external references".to_string()
    } else {
        "deleting this target would break indexed incoming links".to_string()
    };
    let move_risk = if incoming_link_count == 0 {
        "no indexed incoming links require rewrite planning".to_string()
    } else {
        "moving this target requires reviewing affected incoming links before any rewrite"
            .to_string()
    };

    Ok(TargetPathRisk {
        target_path,
        incoming_link_count,
        incoming_links,
        delete_risk,
        move_risk,
    })
}

fn duplicate_stem_risks(index: &GraphIndex) -> Vec<DuplicateStemRisk> {
    let mut by_stem = BTreeMap::<String, Vec<Utf8PathBuf>>::new();
    for document in &index.documents {
        by_stem
            .entry(document.stem.to_lowercase())
            .or_default()
            .push(document.path.clone());
    }
    by_stem
        .into_iter()
        .filter(|(_, paths)| paths.len() > 1)
        .map(|(stem, paths)| DuplicateStemRisk {
            stem,
            paths,
            decision: "duplicate stems make stem-style wikilinks ambiguous; choose explicit paths or rename"
                .to_string(),
        })
        .collect()
}

fn affected_files(
    unresolved_links: &[LinkDecision],
    ambiguous_links: &[LinkDecision],
    path_style_markdown_links: &[LinkDecision],
    target_risk: Option<&TargetPathRisk>,
) -> Vec<Utf8PathBuf> {
    let mut paths = BTreeSet::new();
    for decision in unresolved_links
        .iter()
        .chain(ambiguous_links)
        .chain(path_style_markdown_links)
    {
        paths.insert(decision.source_path.clone());
    }
    if let Some(target_risk) = target_risk {
        for decision in &target_risk.incoming_links {
            paths.insert(decision.source_path.clone());
        }
    }
    paths.into_iter().collect()
}

fn link_decision(link: &Link) -> LinkDecision {
    LinkDecision {
        source_path: link.source_path.clone(),
        raw: link.raw.clone(),
        kind: display::link_kind_str(&link.kind).to_string(),
        target: link.target.clone(),
        anchor: link.anchor.clone(),
        block_ref: link.block_ref.clone(),
        unresolved_reason: link
            .unresolved_reason
            .as_ref()
            .map(|reason| display::unresolved_reason_str(reason).to_string()),
        candidates: link.candidates.clone(),
        decision: decision_for(link),
    }
}

fn decision_for(link: &Link) -> String {
    match link.status {
        LinkStatus::Ambiguous => "skipped: choose one candidate target".to_string(),
        LinkStatus::Unresolved => match link.unresolved_reason {
            Some(UnresolvedReason::AnchorMissing) => {
                "skipped: update heading anchor or target heading".to_string()
            }
            Some(UnresolvedReason::BlockRefMissing) => {
                "skipped: update block reference or target block id".to_string()
            }
            Some(UnresolvedReason::TargetMissing) => {
                "skipped: create target or rewrite link".to_string()
            }
            Some(UnresolvedReason::Ambiguous) | None => {
                "skipped: inspect unresolved link".to_string()
            }
        },
        LinkStatus::Resolved => {
            if link.kind == LinkKind::Markdown {
                "path-style Markdown link; review before moving target paths".to_string()
            } else {
                "resolved link".to_string()
            }
        }
    }
}

