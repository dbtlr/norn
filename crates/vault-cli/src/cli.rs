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
        help = "Run as if vault started in this directory"
    )]
    pub cwd: Option<Utf8PathBuf>,
    #[arg(
        long,
        global = true,
        help = "Run against a named vault from the user registry"
    )]
    pub vault: Option<String>,
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
    #[command(
        about = "Deterministic document search",
        long_about = "Deterministic document search.\n\nSearch reuses document path and frontmatter filters, and adds literal text matching over Markdown file contents. It does not perform semantic, fuzzy, regex, or embedding search."
    )]
    Search(SearchArgs),
    #[command(about = "Manage named vault roots")]
    Registry(RegistryCommand),
    #[command(about = "Plan and apply deterministic vault repairs")]
    Repair(RepairCommand),
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
    #[command(about = "Emit grouped document counts")]
    Summary(DocsSummaryArgs),
    #[command(about = "Emit one document plus incoming, outgoing, and unresolved outgoing links")]
    Inspect(InspectArgs),
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

#[derive(Debug, Parser)]
pub struct RegistryCommand {
    #[command(subcommand)]
    pub command: RegistrySubcommand,
}

#[derive(Debug, Subcommand)]
pub enum RegistrySubcommand {
    #[command(about = "Register a named vault root")]
    Add(RegistryAddArgs),
    #[command(about = "List registered vault roots")]
    List(RegistryListArgs),
    #[command(about = "Remove a registered vault root")]
    Remove(RegistryRemoveArgs),
}

#[derive(Debug, Parser)]
pub struct RegistryAddArgs {
    pub name: String,
    pub path: Utf8PathBuf,
}

#[derive(Debug, Parser)]
pub struct RegistryListArgs {
    #[arg(long, value_enum, help = "Stdout format")]
    pub format: Option<OutputFormat>,
}

#[derive(Debug, Parser)]
pub struct RegistryRemoveArgs {
    pub name: String,
}

#[derive(Debug, Parser)]
pub struct RepairCommand {
    #[command(subcommand)]
    pub command: RepairSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum RepairSubcommand {
    #[command(
        about = "Generate an explicit repair plan from validation findings",
        long_about = "Generate an explicit repair plan from validation findings.\n\nRepair planning is read-only. It uses configured deterministic repair rules and reports unsupported or manual-decision findings explicitly."
    )]
    Plan(RepairPlanArgs),
    #[command(
        about = "Report link and path repair risks without writing files",
        long_about = "Report link and path repair risks without writing files.\n\nThis surfaces unresolved links, ambiguous links, duplicate-stem risks, path-style Markdown links, affected files, and optional move/delete risk for a target."
    )]
    Links(RepairLinksArgs),
    #[command(
        about = "Apply a frontmatter-only repair plan",
        long_about = "Apply a frontmatter-only repair plan.\n\nApply writes by default, checks plan schema and stale file preconditions, preserves Markdown body content, and rejects plans with unsupported or manual-decision findings."
    )]
    Apply(RepairApplyArgs),
}

#[derive(Debug, Parser)]
pub struct RepairPlanArgs {
    #[arg(long, value_enum, default_value_t = RepairOutputFormat::Json, help = "Stdout format")]
    pub format: RepairOutputFormat,
    #[arg(
        long,
        help = "Filter findings by code. Comma-separated values match any listed code"
    )]
    pub code: Vec<String>,
    #[arg(long, help = "Filter findings by severity")]
    pub severity: Vec<String>,
    #[arg(long, help = "Filter findings by frontmatter field")]
    pub field: Vec<String>,
    #[arg(long, help = "Filter findings by validate rule name")]
    pub rule: Vec<String>,
    #[arg(
        long,
        help = "Filter findings by vault-relative path glob using config glob semantics"
    )]
    pub path: Vec<String>,
    #[arg(long, help = "Filter link findings by link target")]
    pub target: Vec<String>,
    #[arg(long, help = "Filter link findings by unresolved reason")]
    pub reason: Vec<String>,
}

#[derive(Debug, Parser)]
pub struct RepairLinksArgs {
    #[arg(
        long,
        help = "Exact vault-relative path or unique document stem to analyze for move/delete risk"
    )]
    pub target: Option<String>,
    #[arg(long, value_enum, default_value_t = RepairOutputFormat::Json, help = "Stdout format")]
    pub format: RepairOutputFormat,
}

#[derive(Debug, Parser)]
pub struct RepairApplyArgs {
    pub plan: Utf8PathBuf,
    #[arg(long, help = "Preview changes without writing files")]
    pub dry_run: bool,
    #[arg(
        long,
        help = "Run validation after apply and report remaining finding counts"
    )]
    pub verify: bool,
    #[arg(long, value_enum, default_value_t = RepairOutputFormat::Json, help = "Stdout format")]
    pub format: RepairOutputFormat,
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
    #[arg(long, value_enum, help = "Stdout format")]
    pub format: Option<OutputFormat>,
}

#[derive(Debug, Parser)]
pub struct DocumentsArgs {
    #[arg(
        long = "filter",
        help = "Frontmatter field:value filter. Comma-separated values match any listed value. Repeat to require multiple fields"
    )]
    pub filters: Vec<String>,
    #[arg(
        long = "path",
        help = "Vault-relative path glob filter using config glob semantics"
    )]
    pub paths: Vec<String>,
    #[arg(long, help = "Require a present, non-null frontmatter field")]
    pub has: Vec<String>,
    #[arg(long, help = "Require a missing or null frontmatter field")]
    pub missing: Vec<String>,
    #[arg(long, value_enum, help = "Stdout format")]
    pub format: Option<OutputFormat>,
}

#[derive(Debug, Parser)]
pub struct GraphArgs {
    #[arg(long, value_enum, help = "Stdout format")]
    pub format: Option<OutputFormat>,
}

#[derive(Debug, Parser)]
pub struct TargetGraphArgs {
    #[arg(
        help = "Exact vault-relative path or unique document stem. Stem matching is case-insensitive"
    )]
    pub target: String,
    #[arg(long, value_enum, help = "Stdout format")]
    pub format: Option<OutputFormat>,
}

#[derive(Debug, Parser)]
pub struct InspectArgs {
    #[arg(
        help = "Exact vault-relative path or unique document stem. Stem matching is case-insensitive"
    )]
    pub target: String,
    #[arg(long, value_enum, default_value_t = OutputFormat::Json, help = "Stdout format")]
    pub format: OutputFormat,
}

#[derive(Debug, Parser)]
pub struct DocsSummaryArgs {
    #[arg(
        long = "count-by",
        help = "Frontmatter field to group document counts by"
    )]
    pub count_by: String,
    #[arg(
        long = "filter",
        help = "Frontmatter field:value filter. Comma-separated values match any listed value. Repeat to require multiple fields"
    )]
    pub filters: Vec<String>,
    #[arg(
        long = "path",
        help = "Vault-relative path glob filter using config glob semantics"
    )]
    pub paths: Vec<String>,
    #[arg(long, help = "Require a present, non-null frontmatter field")]
    pub has: Vec<String>,
    #[arg(long, help = "Require a missing or null frontmatter field")]
    pub missing: Vec<String>,
    #[arg(long, value_enum, help = "Stdout format")]
    pub format: Option<OutputFormat>,
}

#[derive(Debug, Parser)]
pub struct SearchArgs {
    #[arg(
        long = "filter",
        help = "Frontmatter field:value filter. Comma-separated values match any listed value. Repeat to require multiple fields"
    )]
    pub filters: Vec<String>,
    #[arg(
        long = "path",
        help = "Vault-relative path glob filter using config glob semantics"
    )]
    pub paths: Vec<String>,
    #[arg(long, help = "Require a present, non-null frontmatter field")]
    pub has: Vec<String>,
    #[arg(long, help = "Require a missing or null frontmatter field")]
    pub missing: Vec<String>,
    #[arg(
        long,
        help = "Require an exact literal substring in the Markdown file contents. Repeat to require multiple substrings"
    )]
    pub text: Vec<String>,
    #[arg(long, value_enum, help = "Stdout format")]
    pub format: Option<OutputFormat>,
}

#[derive(Debug, Parser)]
pub struct ValidateArgs {
    #[arg(long, value_enum, help = "Stdout format")]
    pub format: Option<OutputFormat>,
    #[arg(
        long,
        help = "Emit grouped validation finding counts instead of raw findings"
    )]
    pub summary: bool,
    #[arg(
        long,
        help = "Filter findings by code. Comma-separated values match any listed code"
    )]
    pub code: Vec<String>,
    #[arg(long, help = "Filter findings by severity")]
    pub severity: Vec<String>,
    #[arg(long, help = "Filter findings by frontmatter field")]
    pub field: Vec<String>,
    #[arg(long, help = "Filter findings by validate rule name")]
    pub rule: Vec<String>,
    #[arg(
        long,
        help = "Filter findings by vault-relative path glob using config glob semantics"
    )]
    pub path: Vec<String>,
    #[arg(long, help = "Filter link findings by link target")]
    pub target: Vec<String>,
    #[arg(long, help = "Filter link findings by unresolved reason")]
    pub reason: Vec<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OutputFormat {
    Json,
    Jsonl,
    Table,
    Paths,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RepairOutputFormat {
    Json,
    Jsonl,
    Table,
}

impl From<RepairOutputFormat> for OutputFormat {
    fn from(format: RepairOutputFormat) -> Self {
        match format {
            RepairOutputFormat::Json => OutputFormat::Json,
            RepairOutputFormat::Jsonl => OutputFormat::Jsonl,
            RepairOutputFormat::Table => OutputFormat::Table,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct InspectOutput {
    pub document: Document,
    pub incoming_links: Vec<Link>,
    pub outgoing_links: Vec<Link>,
    pub unresolved_outgoing_links: Vec<Link>,
}
