mod cli;
mod config;
mod filter;
mod link_repair;
mod output;
mod registry;
mod repair_apply;
mod target;
mod validate_filter;

use std::{collections::BTreeMap, fs, process};

use anyhow::{bail, Result};
use clap::Parser;
use vault_core::{GraphIndex, LinkStatus};
use vault_graph::{build_index_with_options, concise_diagnostics, has_errors};
use vault_standards::{plan_repairs, summarize, validate, RepairPlanFilters};

use crate::cli::{
    Cli, Command, DocsSubcommand, LinksSubcommand, RegistrySubcommand, RepairOutputFormat,
    RepairSubcommand,
};
use crate::config::{effective_cwd, load_config, resolve_path};
use crate::filter::{
    filter_documents, index_frontmatter_keys, summarize_documents, DocumentFilterOptions,
};
use crate::link_repair::plan_link_repairs;
use crate::output::{
    is_broken_pipe, resolve_format, write_document_summary, write_documents, write_files,
    write_findings, write_item_output, write_link_repair_report, write_links, write_output,
    write_repair_apply_report, write_repair_plan, write_validate_summary,
};
use crate::repair_apply::{apply_repair_plan, with_verification};
use crate::target::{
    backlinks, inspect_document, resolve_backlink_target_path, resolve_target_path,
};
use crate::validate_filter::{filter_findings, ValidateFilterOptions};

fn main() {
    let cli = Cli::parse();
    match run(cli) {
        Ok(exit_code) => process::exit(exit_code),
        Err(error) if is_broken_pipe(&error) => process::exit(0),
        Err(error) => {
            eprintln!("{error:#}");
            process::exit(1);
        }
    }
}

fn run(cli: Cli) -> Result<i32> {
    let Cli {
        cwd,
        vault,
        config,
        verbose,
        command,
    } = cli;

    let command = match command {
        Command::Registry(registry_command) => return run_registry(registry_command.command),
        command => command,
    };

    let cwd = effective_cwd(cwd.as_ref(), vault.as_deref())?;
    let config_path = config;

    match command {
        Command::Docs(docs) => match docs.command {
            DocsSubcommand::List(args) => {
                let mut index = build_index_for(&cwd, config_path.as_ref())?;
                trim_diagnostics(&mut index, verbose);
                let options = DocumentFilterOptions {
                    filters: &args.filters.filters,
                    paths: &args.filters.paths,
                    has: &args.filters.has,
                    missing: &args.filters.missing,
                };
                let documents = filter_documents(&index, &options)?;
                write_documents(&documents, resolve_format(args.format))?;
                Ok(exit_code_for(&index))
            }
            DocsSubcommand::Summary(args) => {
                let mut index = build_index_for(&cwd, config_path.as_ref())?;
                trim_diagnostics(&mut index, verbose);
                let options = DocumentFilterOptions {
                    filters: &args.filters.filters,
                    paths: &args.filters.paths,
                    has: &args.filters.has,
                    missing: &args.filters.missing,
                };
                let known_fields = index_frontmatter_keys(&index);
                let documents = filter_documents(&index, &options)?;
                let summary = summarize_documents(&documents, &args.count_by, &known_fields);
                write_document_summary(&summary, resolve_format(args.format))?;
                Ok(exit_code_for(&index))
            }
            DocsSubcommand::Inspect(args) => {
                let mut index = build_index_for(&cwd, config_path.as_ref())?;
                trim_diagnostics(&mut index, verbose);
                let target_path = resolve_target_path(&index, &args.target)?;
                let output = inspect_document(&index, &target_path)?;
                write_item_output(&output, args.format)?;
                Ok(exit_code_for(&index))
            }
        },
        Command::Files(args) => {
            let mut index = build_index_for(&cwd, config_path.as_ref())?;
            trim_diagnostics(&mut index, verbose);
            let files: Vec<_> = index.files.iter().collect();
            write_files(&files, resolve_format(args.format))?;
            Ok(exit_code_for(&index))
        }
        Command::Links(links_command) => match links_command.command {
            LinksSubcommand::List(args) => {
                let mut index = build_index_for(&cwd, config_path.as_ref())?;
                trim_diagnostics(&mut index, verbose);
                let links: Vec<_> = index
                    .documents
                    .iter()
                    .flat_map(|d| d.links.iter())
                    .collect();
                write_links(&links, resolve_format(args.format))?;
                Ok(exit_code_for(&index))
            }
            LinksSubcommand::Unresolved(args) => {
                let mut index = build_index_for(&cwd, config_path.as_ref())?;
                trim_diagnostics(&mut index, verbose);
                let links: Vec<_> = index
                    .documents
                    .iter()
                    .flat_map(|d| d.links.iter())
                    .filter(|l| l.status != LinkStatus::Resolved)
                    .collect();
                write_links(&links, resolve_format(args.format))?;
                Ok(exit_code_for(&index))
            }
            LinksSubcommand::Backlinks(args) => {
                let mut index = build_index_for(&cwd, config_path.as_ref())?;
                trim_diagnostics(&mut index, verbose);
                let target_path = resolve_backlink_target_path(&index, &args.target)?;
                let links = backlinks(&index, &target_path);
                write_links(&links, resolve_format(args.format))?;
                Ok(exit_code_for(&index))
            }
        },
        Command::Search(args) => {
            let mut index = build_index_for(&cwd, config_path.as_ref())?;
            trim_diagnostics(&mut index, verbose);
            let options = DocumentFilterOptions {
                filters: &args.filters.filters,
                paths: &args.filters.paths,
                has: &args.filters.has,
                missing: &args.filters.missing,
            };
            let documents = filter_documents(&index, &options)?;
            let documents = filter_documents_by_text(&cwd, documents, &args.text)?;
            write_documents(&documents, resolve_format(args.format))?;
            Ok(exit_code_for(&index))
        }
        Command::Repair(repair_command) => match repair_command.command {
            RepairSubcommand::Plan(args) => {
                let loaded_config = load_config(&cwd, config_path.as_ref())?;
                let mut index = build_index_with_options(&cwd, &loaded_config.index_options)?;
                trim_diagnostics(&mut index, verbose);
                let findings = validate(&index, &loaded_config.validate);
                let filters = ValidateFilterOptions::from(&args);
                let findings = filter_findings(findings, &filters)?;
                let plan = plan_repairs(
                    cwd.clone(),
                    repair_plan_filters(&args),
                    findings,
                    &loaded_config.repair,
                    &document_hashes(&index),
                );
                if let Some(out) = &args.out {
                    if args.format != RepairOutputFormat::Json {
                        let message = "repair plan --out writes JSON artifacts; \
                            omit --out for table output";
                        bail!(message);
                    }
                    let out_path = resolve_path(&cwd, out);
                    let plan_text = serde_json::to_string_pretty(&plan)?;
                    fs::write(&out_path, format!("{plan_text}\n")).map_err(|error| {
                        anyhow::anyhow!("failed to write repair plan {out_path}: {error}")
                    })?;
                } else {
                    write_repair_plan(&plan, args.format.into())?;
                }
                Ok(exit_code_for(&index))
            }
            RepairSubcommand::Apply(args) => {
                let plan_path = resolve_path(&cwd, &args.plan);
                let plan_text = fs::read_to_string(&plan_path).map_err(|error| {
                    anyhow::anyhow!("failed to read repair plan {plan_path}: {error}")
                })?;
                let plan = serde_json::from_str::<vault_standards::RepairPlan>(&plan_text)
                    .map_err(|error| {
                        anyhow::anyhow!("failed to parse repair plan {plan_path}: {error}")
                    })?;
                let loaded_config = load_config(&cwd, config_path.as_ref())?;
                let mut index = build_index_with_options(&cwd, &loaded_config.index_options)?;
                trim_diagnostics(&mut index, verbose);
                let mut report = apply_repair_plan(&cwd, &index, &plan, args.dry_run)?;
                if args.verify {
                    let mut verify_index =
                        build_index_with_options(&cwd, &loaded_config.index_options)?;
                    trim_diagnostics(&mut verify_index, verbose);
                    let findings = validate(&verify_index, &loaded_config.validate);
                    report = with_verification(report, &findings);
                }
                write_repair_apply_report(&report, args.format.into())?;
                Ok(exit_code_for(&index))
            }
            RepairSubcommand::Links(args) => {
                let mut index = build_index_for(&cwd, config_path.as_ref())?;
                trim_diagnostics(&mut index, verbose);
                let report = plan_link_repairs(&index, args.target.as_deref())?;
                write_link_repair_report(&report, args.format.into())?;
                Ok(exit_code_for(&index))
            }
        },
        Command::Validate(args) => {
            let loaded_config = load_config(&cwd, config_path.as_ref())?;
            let mut index = build_index_with_options(&cwd, &loaded_config.index_options)?;
            trim_diagnostics(&mut index, verbose);
            let findings = validate(&index, &loaded_config.validate);
            let filters = ValidateFilterOptions::from(&args);
            let findings = filter_findings(findings, &filters)?;
            if args.summary {
                let summary = summarize(&findings);
                write_validate_summary(&summary, resolve_format(args.format))?;
            } else {
                write_findings(&findings, resolve_format(args.format))?;
            }
            Ok(exit_code_for(&index))
        }
        Command::Registry(_) => {
            unreachable!("registry commands are handled before vault targeting")
        }
    }
}

fn run_registry(command: RegistrySubcommand) -> Result<i32> {
    match command {
        RegistrySubcommand::Add(args) => {
            registry::add_vault(&args.name, &args.path)?;
            Ok(0)
        }
        RegistrySubcommand::List(args) => {
            let entries = registry::list_vaults()?;
            write_output(&entries, resolve_format(args.format))?;
            Ok(0)
        }
        RegistrySubcommand::Remove(args) => {
            registry::remove_vault(&args.name)?;
            Ok(0)
        }
    }
}

fn repair_plan_filters(args: &crate::cli::RepairPlanArgs) -> RepairPlanFilters {
    RepairPlanFilters {
        code: normalized_filter_values(&args.triage.code),
        severity: normalized_filter_values(&args.triage.severity),
        field: normalized_filter_values(&args.triage.field),
        rule: normalized_filter_values(&args.triage.rule),
        path: normalized_filter_values(&args.triage.path),
        target: normalized_filter_values(&args.triage.target),
        reason: normalized_filter_values(&args.triage.reason),
    }
}

fn normalized_filter_values(values: &[String]) -> Vec<String> {
    values
        .iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn document_hashes(index: &GraphIndex) -> BTreeMap<camino::Utf8PathBuf, String> {
    index
        .documents
        .iter()
        .map(|document| (document.path.clone(), document.hash.clone()))
        .collect()
}

fn filter_documents_by_text<'a>(
    cwd: &camino::Utf8PathBuf,
    documents: Vec<&'a vault_core::Document>,
    text_filters: &[String],
) -> Result<Vec<&'a vault_core::Document>> {
    if text_filters.is_empty() {
        return Ok(documents);
    }

    documents
        .into_iter()
        .map(|document| {
            let path = cwd.join(&document.path);
            let contents = fs::read_to_string(&path)
                .map_err(|error| anyhow::anyhow!("failed to read document {path}: {error}"))?;
            Ok(text_filters
                .iter()
                .all(|needle| contents.contains(needle))
                .then_some(document))
        })
        .filter_map(Result::transpose)
        .collect()
}

fn build_index_for(
    cwd: &camino::Utf8PathBuf,
    config_path: Option<&camino::Utf8PathBuf>,
) -> Result<GraphIndex> {
    let loaded_config = load_config(cwd, config_path)?;
    Ok(build_index_with_options(cwd, &loaded_config.index_options)?)
}

fn trim_diagnostics(index: &mut GraphIndex, verbose: bool) {
    if verbose {
        return;
    }
    for document in &mut index.documents {
        document.diagnostics = concise_diagnostics(document);
    }
}

fn exit_code_for(index: &GraphIndex) -> i32 {
    if has_errors(index) {
        1
    } else {
        0
    }
}
