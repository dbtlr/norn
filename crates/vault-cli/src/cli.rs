use camino::Utf8PathBuf;
use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;
use vault_core::{Document, Link};

#[derive(Debug, Parser)]
#[command(name = "vault")]
#[command(about = "Deterministic Markdown vault graph tools")]
#[command(version)]
pub struct Cli {
    #[arg(
        short = 'C',
        long,
        global = true,
        default_value = ".",
        help = "Run as if vault started in this directory"
    )]
    pub cwd: Utf8PathBuf,
    #[arg(
        long,
        global = true,
        help = "YAML config file. Defaults to <cwd>/.vault/config.yaml when present"
    )]
    pub config: Option<Utf8PathBuf>,
    #[arg(long, global = true, help = "Include full diagnostic detail in output")]
    pub verbose: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(about = "Parsed Markdown documents")]
    Docs(DocsCommand),
    #[command(
        about = "Emit inventoried vault files",
        long_about = "Emit inventoried vault files.\n\nFiles include Markdown documents and non-Markdown attachments. File records can be used with exact-path backlink queries for resolved attachment targets."
    )]
    Files(GraphArgs),
    #[command(about = "Link facts across the vault")]
    Links(LinksCommand),
    #[command(about = "Local SQLite projection of the graph")]
    Cache(CacheCommand),
    #[command(
        about = "Validate vault graph facts and configured frontmatter rules",
        long_about = "Validate vault graph facts and configured frontmatter rules.\n\nValidation reuses graph/index facts to surface unresolved links, ambiguous links, document diagnostics, and configured frontmatter requirements. Validate does not mutate files."
    )]
    Validate(ValidateArgs),
}

#[derive(Debug, Parser)]
pub struct DocsCommand {
    #[command(subcommand)]
    pub command: DocsSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum DocsSubcommand {
    #[command(
        about = "Emit parsed Markdown documents with frontmatter, headings, links, and diagnostics"
    )]
    List(DocumentsArgs),
    #[command(about = "Emit one document plus incoming, outgoing, and unresolved outgoing links")]
    Inspect(TargetGraphArgs),
}

#[derive(Debug, Parser)]
pub struct LinksCommand {
    #[command(subcommand)]
    pub command: LinksSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum LinksSubcommand {
    #[command(
        about = "Emit all parsed link facts",
        long_about = "Emit all parsed link facts.\n\nIncludes body wikilinks, embeds, frontmatter/property wikilinks, URL-decoded Markdown internal links, extensionless Markdown note links, same-note heading/block references, Markdown image links to local files, and links to existing attachments. Use source_context.area and source_context.property to distinguish body links from frontmatter links."
    )]
    List(GraphArgs),
    #[command(
        about = "Emit unresolved and ambiguous link facts",
        long_about = "Emit unresolved and ambiguous link facts.\n\nRows include target-missing, anchor-missing, block-ref-missing, and ambiguous reasons. Ambiguous rows include candidate document paths."
    )]
    Unresolved(GraphArgs),
    #[command(
        about = "Emit incoming links for an exact path or unique stem",
        long_about = "Emit incoming links for an exact vault-relative file path or unique document stem.\n\nExact paths may target Markdown documents or non-Markdown files. Stem matching only applies to Markdown documents and is case-insensitive."
    )]
    Backlinks(TargetGraphArgs),
}

#[derive(Debug, Parser)]
pub struct CacheCommand {
    #[command(subcommand)]
    pub command: CacheSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum CacheSubcommand {
    #[command(
        about = "Write a SQLite graph cache and emit a build summary",
        long_about = "Write a SQLite graph cache and emit a build summary.\n\nThe cache includes inventoried files, parsed Markdown documents, headings, block IDs, graph link facts, and diagnostics. --format only controls stdout; the cache is always SQLite."
    )]
    Build(BuildArgs),
}

#[derive(Debug, Parser)]
pub struct BuildArgs {
    #[arg(
        long,
        help = "SQLite cache file path or directory. Directories receive graph.sqlite; --format only controls stdout"
    )]
    pub cache: Utf8PathBuf,
    #[arg(long, value_enum, default_value_t = OutputFormat::Json, help = "Stdout format")]
    pub format: OutputFormat,
}

#[derive(Debug, Parser)]
pub struct DocumentsArgs {
    #[arg(
        long = "filter",
        help = "Frontmatter-only field:value filter. Repeat to require multiple fields"
    )]
    pub filters: Vec<String>,
    #[arg(long, value_enum, default_value_t = OutputFormat::Jsonl, help = "Stdout format")]
    pub format: OutputFormat,
}

#[derive(Debug, Parser)]
pub struct GraphArgs {
    #[arg(long, value_enum, default_value_t = OutputFormat::Jsonl, help = "Stdout format")]
    pub format: OutputFormat,
}

#[derive(Debug, Parser)]
pub struct TargetGraphArgs {
    #[arg(
        help = "Exact vault-relative path or unique document stem. Stem matching is case-insensitive"
    )]
    pub target: String,
    #[arg(long, value_enum, default_value_t = OutputFormat::Jsonl, help = "Stdout format")]
    pub format: OutputFormat,
}

#[derive(Debug, Parser)]
pub struct ValidateArgs {
    #[arg(long, value_enum, default_value_t = OutputFormat::Jsonl, help = "Stdout format")]
    pub format: OutputFormat,
    #[arg(
        long,
        help = "Emit grouped validation finding counts instead of raw findings"
    )]
    pub summary: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OutputFormat {
    Json,
    Jsonl,
}

#[derive(Debug, Serialize)]
pub struct InspectOutput {
    pub document: Document,
    pub incoming_links: Vec<Link>,
    pub outgoing_links: Vec<Link>,
    pub unresolved_outgoing_links: Vec<Link>,
}
