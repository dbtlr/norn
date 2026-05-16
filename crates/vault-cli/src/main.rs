use anyhow::{bail, Result};
use camino::Utf8PathBuf;
use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;
use vault_core::{Document, GraphIndex, Link, LinkStatus};
use vault_index::{build_index, concise_diagnostics, has_errors};

#[derive(Debug, Parser)]
#[command(name = "vault")]
#[command(about = "Deterministic Markdown vault graph tools")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Graph(GraphCommand),
}

#[derive(Debug, Parser)]
struct GraphCommand {
    #[command(subcommand)]
    command: GraphSubcommand,
}

#[derive(Debug, Subcommand)]
enum GraphSubcommand {
    Documents(GraphArgs),
    Links(GraphArgs),
    Unresolved(GraphArgs),
    Backlinks(TargetGraphArgs),
    Inspect(TargetGraphArgs),
}

#[derive(Debug, Parser)]
struct GraphArgs {
    #[arg(long, default_value = ".")]
    root: Utf8PathBuf,
    #[arg(long, value_enum, default_value_t = OutputFormat::Jsonl)]
    format: OutputFormat,
    #[arg(long)]
    verbose: bool,
}

#[derive(Debug, Parser)]
struct TargetGraphArgs {
    target: String,
    #[arg(long, default_value = ".")]
    root: Utf8PathBuf,
    #[arg(long, value_enum, default_value_t = OutputFormat::Jsonl)]
    format: OutputFormat,
    #[arg(long)]
    verbose: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Json,
    Jsonl,
}

#[derive(Debug, Serialize)]
struct InspectOutput {
    document: Document,
    incoming_links: Vec<Link>,
    outgoing_links: Vec<Link>,
    unresolved_outgoing_links: Vec<Link>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let exit_code = run(cli)?;
    std::process::exit(exit_code);
}

fn run(cli: Cli) -> Result<i32> {
    match cli.command {
        Command::Graph(graph) => match graph.command {
            GraphSubcommand::Documents(args) => {
                let mut index = build_index(&args.root)?;
                trim_diagnostics(&mut index, args.verbose);
                write_output(&index.documents, args.format)?;
                Ok(exit_code_for(&index))
            }
            GraphSubcommand::Links(args) => {
                let mut index = build_index(&args.root)?;
                trim_diagnostics(&mut index, args.verbose);
                let links = all_links(&index);
                write_output(&links, args.format)?;
                Ok(exit_code_for(&index))
            }
            GraphSubcommand::Unresolved(args) => {
                let mut index = build_index(&args.root)?;
                trim_diagnostics(&mut index, args.verbose);
                let links = unresolved_links(&index);
                write_output(&links, args.format)?;
                Ok(exit_code_for(&index))
            }
            GraphSubcommand::Backlinks(args) => {
                let mut index = build_index(&args.root)?;
                trim_diagnostics(&mut index, args.verbose);
                let target_path = resolve_target_path(&index, &args.target)?;
                let links = backlinks(&index, &target_path);
                write_output(&links, args.format)?;
                Ok(exit_code_for(&index))
            }
            GraphSubcommand::Inspect(args) => {
                let mut index = build_index(&args.root)?;
                trim_diagnostics(&mut index, args.verbose);
                let target_path = resolve_target_path(&index, &args.target)?;
                let output = inspect_document(&index, &target_path)?;
                write_item_output(&output, args.format)?;
                Ok(exit_code_for(&index))
            }
        },
    }
}

fn trim_diagnostics(index: &mut GraphIndex, verbose: bool) {
    if verbose {
        return;
    }

    for document in &mut index.documents {
        document.diagnostics = concise_diagnostics(document);
    }
}

fn all_links(index: &GraphIndex) -> Vec<&Link> {
    index
        .documents
        .iter()
        .flat_map(|document| document.links.iter())
        .collect()
}

fn unresolved_links(index: &GraphIndex) -> Vec<&Link> {
    index
        .documents
        .iter()
        .flat_map(|document| document.links.iter())
        .filter(|link| link.status != LinkStatus::Resolved)
        .collect()
}

fn backlinks<'a>(index: &'a GraphIndex, target_path: &Utf8PathBuf) -> Vec<&'a Link> {
    index
        .documents
        .iter()
        .flat_map(|document| document.links.iter())
        .filter(|link| link.resolved_path.as_ref() == Some(target_path))
        .collect()
}

fn resolve_target_path(index: &GraphIndex, target: &str) -> Result<Utf8PathBuf> {
    if let Some(document) = index
        .documents
        .iter()
        .find(|document| document.path == target)
    {
        return Ok(document.path.clone());
    }

    let matches = index
        .documents
        .iter()
        .filter(|document| document.stem == target)
        .map(|document| document.path.clone())
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [path] => Ok(path.clone()),
        [] => bail!("no document matched path or stem: {target}"),
        many => bail!(
            "ambiguous document stem: {target}; candidates: {}",
            many.iter()
                .map(|path| path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn inspect_document(index: &GraphIndex, target_path: &Utf8PathBuf) -> Result<InspectOutput> {
    let document = index
        .documents
        .iter()
        .find(|document| &document.path == target_path)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("document not found after resolution: {target_path}"))?;

    let incoming_links = backlinks(index, target_path)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let outgoing_links = document.links.clone();
    let unresolved_outgoing_links = document
        .links
        .iter()
        .filter(|link| link.status != LinkStatus::Resolved)
        .cloned()
        .collect::<Vec<_>>();

    Ok(InspectOutput {
        document,
        incoming_links,
        outgoing_links,
        unresolved_outgoing_links,
    })
}

fn write_output<T: Serialize>(items: &[T], format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(items)?);
        }
        OutputFormat::Jsonl => {
            for item in items {
                println!("{}", serde_json::to_string(item)?);
            }
        }
    }
    Ok(())
}

fn write_item_output<T: Serialize>(item: &T, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(item)?);
        }
        OutputFormat::Jsonl => {
            println!("{}", serde_json::to_string(item)?);
        }
    }
    Ok(())
}

fn exit_code_for(index: &GraphIndex) -> i32 {
    if has_errors(index) {
        1
    } else {
        0
    }
}
