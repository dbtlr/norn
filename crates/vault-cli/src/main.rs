mod cli;
mod config;
mod filter;
mod output;
mod target;
mod validate_filter;

use std::process;

use anyhow::Result;
use clap::Parser;
use vault_core::{GraphIndex, LinkStatus};
use vault_graph::{build_index_with_options, concise_diagnostics, has_errors, write_sqlite_cache};
use vault_standards::{summarize, validate};

use crate::cli::{CacheSubcommand, Cli, Command, DocsSubcommand, LinksSubcommand};
use crate::config::{effective_cwd, load_config, resolve_path};
use crate::filter::{
    filter_documents, index_frontmatter_keys, summarize_documents, DocumentFilterOptions,
};
use crate::output::{is_broken_pipe, write_item_output, write_output};
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
    let cwd = effective_cwd(&cli.cwd)?;
    let config_path = cli.config;
    let verbose = cli.verbose;

    match cli.command {
        Command::Docs(docs) => match docs.command {
            DocsSubcommand::List(args) => {
                let mut index = build_index_for(&cwd, config_path.as_ref())?;
                trim_diagnostics(&mut index, verbose);
                let options = DocumentFilterOptions {
                    filters: &args.filters,
                    paths: &args.paths,
                    has: &args.has,
                    missing: &args.missing,
                };
                let documents = filter_documents(&index, &options)?;
                write_output(&documents, args.format)?;
                Ok(exit_code_for(&index))
            }
            DocsSubcommand::Summary(args) => {
                let mut index = build_index_for(&cwd, config_path.as_ref())?;
                trim_diagnostics(&mut index, verbose);
                let options = DocumentFilterOptions {
                    filters: &args.filters,
                    paths: &args.paths,
                    has: &args.has,
                    missing: &args.missing,
                };
                let known_fields = index_frontmatter_keys(&index);
                let documents = filter_documents(&index, &options)?;
                let summary = summarize_documents(&documents, &args.count_by, &known_fields);
                write_item_output(&summary, args.format)?;
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
            write_output(&files, args.format)?;
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
                write_output(&links, args.format)?;
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
                write_output(&links, args.format)?;
                Ok(exit_code_for(&index))
            }
            LinksSubcommand::Backlinks(args) => {
                let mut index = build_index_for(&cwd, config_path.as_ref())?;
                trim_diagnostics(&mut index, verbose);
                let target_path = resolve_backlink_target_path(&index, &args.target)?;
                let links = backlinks(&index, &target_path);
                write_output(&links, args.format)?;
                Ok(exit_code_for(&index))
            }
        },
        Command::Cache(cache) => match cache.command {
            CacheSubcommand::Build(args) => {
                let mut index = build_index_for(&cwd, config_path.as_ref())?;
                trim_diagnostics(&mut index, verbose);
                let cache_path = resolve_path(&cwd, &args.cache);
                let summary = write_sqlite_cache(&index, &cache_path)?;
                write_item_output(&summary, args.format)?;
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
                write_item_output(&summary, args.format)?;
            } else {
                write_output(&findings, args.format)?;
            }
            Ok(exit_code_for(&index))
        }
    }
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
