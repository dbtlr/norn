mod cli;
mod config;
mod filter;
mod output;
mod target;

use std::process;

use anyhow::Result;
use clap::Parser;
use vault_core::{GraphIndex, LinkStatus};
use vault_graph::{build_index_with_options, concise_diagnostics, has_errors, write_sqlite_cache};
use vault_standards::{summarize, validate};

use crate::cli::{Cli, Command, GraphSubcommand};
use crate::config::{effective_cwd, load_config, resolve_path};
use crate::filter::filter_documents;
use crate::output::{is_broken_pipe, write_item_output, write_output};
use crate::target::{
    backlinks, inspect_document, resolve_backlink_target_path, resolve_target_path,
};

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

    match cli.command {
        Command::Graph(graph) => match graph.command {
            GraphSubcommand::Build(args) => {
                let mut index = build_index_for(&cwd, args.config.as_ref())?;
                trim_diagnostics(&mut index, args.verbose);
                let cache_path = resolve_path(&cwd, &args.cache);
                let summary = write_sqlite_cache(&index, &cache_path)?;
                write_item_output(&summary, args.format)?;
                Ok(exit_code_for(&index))
            }
            GraphSubcommand::Documents(args) => {
                let mut index = build_index_for(&cwd, args.config.as_ref())?;
                trim_diagnostics(&mut index, args.verbose);
                let documents = filter_documents(&index, &args.filters)?;
                write_output(&documents, args.format)?;
                Ok(exit_code_for(&index))
            }
            GraphSubcommand::Links(args) => {
                let mut index = build_index_for(&cwd, args.config.as_ref())?;
                trim_diagnostics(&mut index, args.verbose);
                let links: Vec<_> = index
                    .documents
                    .iter()
                    .flat_map(|d| d.links.iter())
                    .collect();
                write_output(&links, args.format)?;
                Ok(exit_code_for(&index))
            }
            GraphSubcommand::Files(args) => {
                let mut index = build_index_for(&cwd, args.config.as_ref())?;
                trim_diagnostics(&mut index, args.verbose);
                let files: Vec<_> = index.files.iter().collect();
                write_output(&files, args.format)?;
                Ok(exit_code_for(&index))
            }
            GraphSubcommand::Unresolved(args) => {
                let mut index = build_index_for(&cwd, args.config.as_ref())?;
                trim_diagnostics(&mut index, args.verbose);
                let links: Vec<_> = index
                    .documents
                    .iter()
                    .flat_map(|d| d.links.iter())
                    .filter(|l| l.status != LinkStatus::Resolved)
                    .collect();
                write_output(&links, args.format)?;
                Ok(exit_code_for(&index))
            }
            GraphSubcommand::Diagnostics(args) => {
                let mut index = build_index_for(&cwd, args.config.as_ref())?;
                trim_diagnostics(&mut index, args.verbose);
                let diagnostics: Vec<_> = index
                    .documents
                    .iter()
                    .flat_map(|d| {
                        d.diagnostics.iter().cloned().map(move |diagnostic| {
                            crate::cli::DocumentDiagnostic {
                                path: d.path.clone(),
                                diagnostic,
                            }
                        })
                    })
                    .collect();
                write_output(&diagnostics, args.format)?;
                Ok(exit_code_for(&index))
            }
            GraphSubcommand::Backlinks(args) => {
                let mut index = build_index_for(&cwd, args.config.as_ref())?;
                trim_diagnostics(&mut index, args.verbose);
                let target_path = resolve_backlink_target_path(&index, &args.target)?;
                let links = backlinks(&index, &target_path);
                write_output(&links, args.format)?;
                Ok(exit_code_for(&index))
            }
            GraphSubcommand::Inspect(args) => {
                let mut index = build_index_for(&cwd, args.config.as_ref())?;
                trim_diagnostics(&mut index, args.verbose);
                let target_path = resolve_target_path(&index, &args.target)?;
                let output = inspect_document(&index, &target_path)?;
                write_item_output(&output, args.format)?;
                Ok(exit_code_for(&index))
            }
        },
        Command::Validate(args) => {
            let loaded_config = load_config(&cwd, args.config.as_ref())?;
            let mut index = build_index_with_options(&cwd, &loaded_config.index_options)?;
            trim_diagnostics(&mut index, args.verbose);
            let findings = validate(&index, &loaded_config.validate);
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
