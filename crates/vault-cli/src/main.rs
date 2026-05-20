mod cache;
mod cli;
mod completions;
mod config;
mod config_loader;
mod filter;
mod find;
mod init;
mod init_scan;
mod link_repair;
mod output;
mod query;
mod repair_apply;
mod target;
mod validate_filter;

use std::{fs, process};

use anyhow::{bail, Result};
use clap::Parser;
use vault_core::{GraphIndex, LinkStatus};
use vault_graph::{concise_diagnostics, has_errors};
use vault_standards::{plan_repairs, summarize, validate, RepairPlanFilters};

use crate::cli::{
    CacheSubcommand, Cli, Command, ConfigSubcommand, DocsSubcommand, LinksSubcommand,
    RepairOutputFormat, RepairSubcommand,
};
use crate::config_loader::{effective_cwd, load_config, resolve_path};
use crate::filter::{
    filter_documents, index_frontmatter_keys, summarize_documents, DocumentFilterOptions,
};
use crate::link_repair::plan_link_repairs;
use crate::output::legacy::{
    is_broken_pipe, resolve_format, write_document_summary, write_files, write_findings,
    write_item_output, write_link_repair_report, write_links, write_repair_apply_report,
    write_repair_plan, write_validate_summary,
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
        config,
        verbose,
        no_cache_refresh,
        color,
        command,
    } = cli;

    let command = match command {
        Command::Completions(args) => return run_completions_command(args),
        Command::Manpage => return run_manpage_command(),
        command => command,
    };

    let cwd = effective_cwd(cwd.as_ref())?;
    let config_path = config;

    match command {
        Command::Docs(docs) => match docs.command {
            DocsSubcommand::Summary(args) => {
                let mut index = build_index_for(&cwd, config_path.as_ref(), no_cache_refresh)?;
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
                let mut index = build_index_for(&cwd, config_path.as_ref(), no_cache_refresh)?;
                trim_diagnostics(&mut index, verbose);
                let target_path = resolve_target_path(&index, &args.target)?;
                let output = inspect_document(&index, &target_path)?;
                write_item_output(&output, args.format)?;
                Ok(exit_code_for(&index))
            }
        },
        Command::Files(args) => {
            let mut index = build_index_for(&cwd, config_path.as_ref(), no_cache_refresh)?;
            trim_diagnostics(&mut index, verbose);
            let files: Vec<_> = index.files.iter().collect();
            write_files(&files, resolve_format(args.format))?;
            Ok(exit_code_for(&index))
        }
        Command::Links(links_command) => match links_command.command {
            LinksSubcommand::List(args) => {
                let mut index = build_index_for(&cwd, config_path.as_ref(), no_cache_refresh)?;
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
                let mut index = build_index_for(&cwd, config_path.as_ref(), no_cache_refresh)?;
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
                let mut index = build_index_for(&cwd, config_path.as_ref(), no_cache_refresh)?;
                trim_diagnostics(&mut index, verbose);
                let target_path = resolve_backlink_target_path(&index, &args.target)?;
                let links = backlinks(&index, &target_path);
                write_links(&links, resolve_format(args.format))?;
                Ok(exit_code_for(&index))
            }
        },
        Command::Repair(repair_command) => match repair_command.command {
            RepairSubcommand::Plan(args) => {
                let loaded_config = load_config(&cwd, config_path.as_ref())?;
                let mut index = crate::cache::load_graph_index(
                    &cwd,
                    &loaded_config.index_options,
                    no_cache_refresh,
                )?;
                trim_diagnostics(&mut index, verbose);
                let findings = validate(&index, &loaded_config.validate);
                let filters = ValidateFilterOptions::from(&args);
                let findings = filter_findings(findings, &filters)?;
                let plan = plan_repairs(
                    cwd.clone(),
                    repair_plan_filters(&args),
                    findings,
                    &loaded_config.repair,
                    &index,
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
                let mut index = crate::cache::load_graph_index(
                    &cwd,
                    &loaded_config.index_options,
                    no_cache_refresh,
                )?;
                trim_diagnostics(&mut index, verbose);
                let mut report = apply_repair_plan(&cwd, &index, &plan, args.dry_run)?;
                if args.verify {
                    // After apply, files on disk changed — force a refresh
                    // regardless of the user's flag so verification reflects
                    // the post-apply state.
                    let mut verify_index =
                        crate::cache::load_graph_index(&cwd, &loaded_config.index_options, false)?;
                    trim_diagnostics(&mut verify_index, verbose);
                    let findings = validate(&verify_index, &loaded_config.validate);
                    report = with_verification(report, &findings);
                }
                write_repair_apply_report(&report, args.format.into())?;
                Ok(exit_code_for(&index))
            }
            RepairSubcommand::Links(args) => {
                let mut index = build_index_for(&cwd, config_path.as_ref(), no_cache_refresh)?;
                trim_diagnostics(&mut index, verbose);
                let report =
                    plan_link_repairs(&index, args.target.as_deref(), args.move_to.as_deref())?;
                write_link_repair_report(&report, args.format.into())?;
                Ok(exit_code_for(&index))
            }
        },
        Command::Cache(cache_command) => {
            match &cache_command.command {
                CacheSubcommand::Index(args) => crate::cache::run_index(&cwd, args)?,
                CacheSubcommand::Rebuild => crate::cache::run_rebuild(&cwd)?,
                CacheSubcommand::Clear => crate::cache::run_clear(&cwd)?,
                CacheSubcommand::Status(args) => crate::cache::run_status(&cwd, args)?,
            }
            Ok(0)
        }
        Command::Config(cfg) => match cfg.command {
            ConfigSubcommand::Show(args) => {
                crate::config::run_show(&cwd, config_path.as_ref(), &args, color)
            }
            ConfigSubcommand::Validate(args) => {
                crate::config::run_validate(&cwd, config_path.as_ref(), &args, color)
            }
            ConfigSubcommand::Migrate => crate::config::run_migrate(&cwd, config_path.as_ref()),
            ConfigSubcommand::Edit(args) => {
                crate::config::run_edit(&cwd, config_path.as_ref(), &args, color)
            }
        },
        Command::Validate(args) => {
            let loaded_config = load_config(&cwd, config_path.as_ref())?;
            let mut index = crate::cache::load_graph_index(
                &cwd,
                &loaded_config.index_options,
                no_cache_refresh,
            )?;
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
        Command::Find(args) => find::run(args, &cwd, no_cache_refresh, color),
        Command::Init(args) => init::run(&cwd, &args),
        Command::Completions(_) => {
            unreachable!("completions are handled before vault targeting")
        }
        Command::Manpage => {
            unreachable!("manpage is handled before vault targeting")
        }
    }
}

fn run_completions_command(cmd: crate::cli::CompletionsCommand) -> Result<i32> {
    match cmd.command {
        crate::cli::CompletionsSubcommand::Init(args) => {
            completions::run_init(args.shell)?;
            Ok(0)
        }
        crate::cli::CompletionsSubcommand::Install(args) => {
            completions::run_install(args)?;
            Ok(0)
        }
    }
}

fn run_manpage_command() -> Result<i32> {
    completions::run_manpage()?;
    Ok(0)
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

fn build_index_for(
    cwd: &camino::Utf8PathBuf,
    config_path: Option<&camino::Utf8PathBuf>,
    no_cache_refresh: bool,
) -> Result<GraphIndex> {
    let loaded_config = load_config(cwd, config_path)?;
    crate::cache::load_graph_index(cwd, &loaded_config.index_options, no_cache_refresh)
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
