use anyhow::{bail, Result};
use camino::Utf8PathBuf;
use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;
use vault_core::{Diagnostic, Document, GraphIndex, Link, LinkStatus};
use vault_index::{build_index, concise_diagnostics, has_errors, write_sqlite_cache};

#[derive(Debug, Parser)]
#[command(name = "vault")]
#[command(about = "Deterministic Markdown vault graph tools")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Query and cache derived Markdown vault graph facts")]
    Graph(GraphCommand),
}

#[derive(Debug, Parser)]
#[command(about = "Read-only graph/index commands for Markdown vaults")]
struct GraphCommand {
    #[command(subcommand)]
    command: GraphSubcommand,
}

#[derive(Debug, Subcommand)]
enum GraphSubcommand {
    #[command(about = "Write a SQLite graph cache and emit a build summary")]
    Build(BuildArgs),
    #[command(
        about = "Emit parsed Markdown documents with frontmatter, headings, links, and diagnostics"
    )]
    Documents(DocumentsArgs),
    #[command(about = "Emit all parsed link facts")]
    Links(GraphArgs),
    #[command(about = "Emit unresolved and ambiguous link facts")]
    Unresolved(GraphArgs),
    #[command(about = "Emit document parse diagnostics")]
    Diagnostics(GraphArgs),
    #[command(about = "Emit incoming links for an exact path or unique stem")]
    Backlinks(TargetGraphArgs),
    #[command(about = "Emit one document plus incoming, outgoing, and unresolved outgoing links")]
    Inspect(TargetGraphArgs),
}

#[derive(Debug, Parser)]
struct BuildArgs {
    #[arg(long, default_value = ".", help = "Vault root to index")]
    root: Utf8PathBuf,
    #[arg(
        long,
        help = "SQLite cache file path or directory. Directories receive graph.sqlite; --format only controls stdout"
    )]
    cache: Utf8PathBuf,
    #[arg(long, value_enum, default_value_t = OutputFormat::Json, help = "Stdout format")]
    format: OutputFormat,
    #[arg(long, help = "Include verbose diagnostic details")]
    verbose: bool,
}

#[derive(Debug, Parser)]
struct DocumentsArgs {
    #[arg(long, default_value = ".", help = "Vault root to index")]
    root: Utf8PathBuf,
    #[arg(
        long = "filter",
        help = "Frontmatter-only field:value filter. Repeat to require multiple fields"
    )]
    filters: Vec<String>,
    #[arg(long, value_enum, default_value_t = OutputFormat::Jsonl, help = "Stdout format")]
    format: OutputFormat,
    #[arg(long, help = "Include verbose diagnostic details")]
    verbose: bool,
}

#[derive(Debug, Parser)]
struct GraphArgs {
    #[arg(long, default_value = ".", help = "Vault root to index")]
    root: Utf8PathBuf,
    #[arg(long, value_enum, default_value_t = OutputFormat::Jsonl, help = "Stdout format")]
    format: OutputFormat,
    #[arg(long, help = "Include verbose diagnostic details")]
    verbose: bool,
}

#[derive(Debug, Parser)]
struct TargetGraphArgs {
    #[arg(
        help = "Exact vault-relative path or unique document stem. Stem matching is case-insensitive"
    )]
    target: String,
    #[arg(long, default_value = ".", help = "Vault root to index")]
    root: Utf8PathBuf,
    #[arg(long, value_enum, default_value_t = OutputFormat::Jsonl, help = "Stdout format")]
    format: OutputFormat,
    #[arg(long, help = "Include verbose diagnostic details")]
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

#[derive(Debug, Serialize)]
struct DocumentDiagnostic {
    path: Utf8PathBuf,
    diagnostic: Diagnostic,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let exit_code = run(cli)?;
    std::process::exit(exit_code);
}

fn run(cli: Cli) -> Result<i32> {
    match cli.command {
        Command::Graph(graph) => match graph.command {
            GraphSubcommand::Build(args) => {
                let mut index = build_index(&args.root)?;
                trim_diagnostics(&mut index, args.verbose);
                let summary = write_sqlite_cache(&index, &args.cache)?;
                write_item_output(&summary, args.format)?;
                Ok(exit_code_for(&index))
            }
            GraphSubcommand::Documents(args) => {
                let mut index = build_index(&args.root)?;
                trim_diagnostics(&mut index, args.verbose);
                let documents = filter_documents(&index, &args.filters)?;
                write_output(&documents, args.format)?;
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
            GraphSubcommand::Diagnostics(args) => {
                let mut index = build_index(&args.root)?;
                trim_diagnostics(&mut index, args.verbose);
                let diagnostics = all_diagnostics(&index);
                write_output(&diagnostics, args.format)?;
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

fn all_diagnostics(index: &GraphIndex) -> Vec<DocumentDiagnostic> {
    index
        .documents
        .iter()
        .flat_map(|document| {
            document
                .diagnostics
                .iter()
                .cloned()
                .map(|diagnostic| DocumentDiagnostic {
                    path: document.path.clone(),
                    diagnostic,
                })
        })
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
        .filter(|document| document.stem.eq_ignore_ascii_case(target))
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

fn filter_documents<'a>(index: &'a GraphIndex, filters: &[String]) -> Result<Vec<&'a Document>> {
    let parsed_filters = filters
        .iter()
        .map(|filter| parse_filter(filter))
        .collect::<Result<Vec<_>>>()?;

    Ok(index
        .documents
        .iter()
        .filter(|document| {
            parsed_filters
                .iter()
                .all(|(field, expected)| frontmatter_matches(document, field, expected))
        })
        .collect())
}

fn parse_filter(filter: &str) -> Result<(String, String)> {
    let Some((field, value)) = filter.split_once(':') else {
        bail!("invalid filter, expected field:value: {filter}");
    };

    let field = field.trim();
    let value = value.trim();
    if field.is_empty() || value.is_empty() {
        bail!("invalid filter, expected non-empty field and value: {filter}");
    }

    Ok((field.to_string(), value.to_string()))
}

fn frontmatter_matches(document: &Document, field: &str, expected: &str) -> bool {
    let Some(frontmatter) = &document.frontmatter else {
        return false;
    };
    let Some(value) = frontmatter.get(field) else {
        return false;
    };

    match value {
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| scalar_value_matches(value, expected)),
        other => scalar_value_matches(other, expected),
    }
}

fn scalar_value_matches(value: &serde_json::Value, expected: &str) -> bool {
    match value {
        serde_json::Value::String(actual) => actual == expected,
        serde_json::Value::Bool(actual) => actual.to_string() == expected,
        serde_json::Value::Number(actual) => actual.to_string() == expected,
        _ => false,
    }
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
