use camino::Utf8PathBuf;
use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "vault")]
#[command(about = "Deterministic Markdown vault graph tools")]
#[command(version)]
pub struct Cli {
    #[arg(
        short = 'C',
        long,
        global = true,
        help_heading = "Global options",
        help = "Run as if vault started in this directory"
    )]
    pub cwd: Option<Utf8PathBuf>,
    #[arg(
        long,
        global = true,
        help_heading = "Global options",
        help = "YAML config file. Defaults to <cwd>/.vault/config.yaml when present"
    )]
    pub config: Option<Utf8PathBuf>,
    #[arg(
        long,
        global = true,
        help_heading = "Global options",
        help = "Include full diagnostic detail in output"
    )]
    pub verbose: bool,
    #[arg(
        long = "no-cache-refresh",
        global = true,
        help_heading = "Global options",
        help = "Skip the implicit cache refresh that query commands run before reading the graph index"
    )]
    pub no_cache_refresh: bool,
    #[arg(
        long,
        global = true,
        value_enum,
        default_value = "auto",
        help_heading = "Global options",
        help = "Color output. Honors NO_COLOR / CLICOLOR_FORCE."
    )]
    pub color: ColorWhen,
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
    #[command(
        about = "Find documents in the vault — full-text + metadata filters with sort/limit/paging"
    )]
    Find(FindArgs),
    #[command(about = "Scaffold .vault/config.yaml")]
    Init(InitArgs),
    #[command(about = "Link facts across the vault")]
    Links(LinksCommand),
    #[command(about = "Plan and apply deterministic vault repairs")]
    Repair(RepairCommand),
    #[command(
        about = "Validate vault graph facts and configured frontmatter rules",
        long_about = "Validate vault graph facts and configured frontmatter rules.\n\nValidation reuses graph/index facts to surface unresolved links, ambiguous links, document diagnostics, and configured frontmatter requirements. Validate does not mutate files."
    )]
    Validate(ValidateArgs),
    #[command(about = "Shell completion installation and script emission")]
    Completions(CompletionsCommand),
    #[command(
        about = "Manage the SQLite-backed vault graph cache",
        long_about = "Manage the SQLite-backed vault graph cache.\n\nThe cache is a per-vault disposable read-acceleration store. Query commands open it transparently; these subcommands let you index, rebuild, clear, or inspect it explicitly."
    )]
    Cache(CacheCommand),
    #[command(about = "Manage the per-vault `.vault/config.yaml`")]
    Config(ConfigCommand),
    #[command(hide = true, about = "Emit roff-format man page to stdout")]
    Manpage,
}

#[derive(Debug, Parser)]
pub struct CacheCommand {
    #[command(subcommand)]
    pub command: CacheSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum CacheSubcommand {
    #[command(
        about = "Update the cache incrementally",
        long_about = "Update the cache incrementally.\n\nDetects changed files via mtime+size and re-parses only the affected documents. Pass --rebuild to force a full from-scratch rebuild, or --force-hash to bypass the cheap-check and hash every file."
    )]
    Index(CacheIndexArgs),
    #[command(about = "Rebuild the cache from scratch")]
    Rebuild,
    #[command(
        about = "Delete the cache database",
        long_about = "Delete the cache database.\n\nRemoves the cache.db file and its WAL/SHM siblings. The next cache-aware command auto-recreates a fresh database."
    )]
    Clear,
    #[command(about = "Show cache path, size, document and link counts, and schema version")]
    Status(CacheStatusArgs),
}

#[derive(Debug, Parser)]
pub struct CacheIndexArgs {
    #[arg(
        long,
        help = "Rebuild the cache from scratch instead of an incremental update"
    )]
    pub rebuild: bool,
    #[arg(
        long = "force-hash",
        help = "Skip the mtime+size cheap-check and hash every file"
    )]
    pub force_hash: bool,
}

#[derive(Debug, Parser)]
pub struct CacheStatusArgs {
    #[arg(long, value_enum, default_value_t = CacheOutputFormat::Text, help = "Stdout format")]
    pub format: CacheOutputFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CacheOutputFormat {
    Text,
    Json,
}

#[derive(Debug, Parser)]
pub struct DocsCommand {
    #[command(subcommand)]
    pub command: DocsSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum DocsSubcommand {
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
        long_about = "Emit all parsed link facts.\n\nIncludes body wikilinks, embeds, frontmatter/property wikilinks, URL-decoded Markdown internal links, extensionless Markdown note links, same-note heading/block references, Markdown image links to local files, and links to existing attachments. Use source_context.area and source_context.property to distinguish body links from frontmatter links.\n\n--format paths emits unique source paths; multiple links from the same source appear once."
    )]
    List(GraphArgs),
    #[command(
        about = "Emit unresolved and ambiguous link facts",
        long_about = "Emit unresolved and ambiguous link facts.\n\nRows include target-missing, anchor-missing, block-ref-missing, and ambiguous reasons. Ambiguous rows include candidate document paths.\n\n--format paths emits unique source paths."
    )]
    Unresolved(GraphArgs),
    #[command(
        about = "Emit incoming links for an exact path or unique stem",
        long_about = "Emit incoming links for an exact vault-relative file path or unique document stem.\n\nExact paths may target Markdown documents or non-Markdown files. Stem matching only applies to Markdown documents and is case-insensitive.\n\n--format paths emits unique source paths."
    )]
    Backlinks(TargetGraphArgs),
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
        long_about = "Generate an explicit repair plan from validation findings.\n\nRepair planning is read-only. It uses configured deterministic repair rules to produce applyable changes, and reports skipped, unsupported, and ambiguous findings as non-blocking planning fallout."
    )]
    Plan(RepairPlanArgs),
    #[command(
        about = "Report link and path repair risks without writing files",
        long_about = "Report link and path repair risks without writing files.\n\nThis surfaces unresolved links, ambiguous links, duplicate-stem risks, path-style Markdown links, affected files, and optional move/delete risk for a target."
    )]
    Links(RepairLinksArgs),
    #[command(
        about = "Apply a frontmatter-only repair plan",
        long_about = "Apply a frontmatter-only repair plan.\n\nApply writes by default, executes deterministic changes, reports skipped fallout as context, preserves Markdown body content, and rejects unsupported schemas, stale hashes, expected-old-value mismatches, conflicting changes, and unsupported operations."
    )]
    Apply(RepairApplyArgs),
}

#[derive(Debug, Clone, clap::Args)]
pub struct FrontmatterFilterArgs {
    #[arg(
        long = "filter",
        help_heading = "Filter options",
        help = "Frontmatter field:value filter. Comma-separated values match any listed value. Repeat to require multiple fields"
    )]
    pub filters: Vec<String>,
    #[arg(
        long = "path",
        help_heading = "Filter options",
        help = "Vault-relative path glob filter using config glob semantics"
    )]
    pub paths: Vec<String>,
    #[arg(
        long,
        help_heading = "Filter options",
        help = "Require a present, non-null frontmatter field"
    )]
    pub has: Vec<String>,
    #[arg(
        long,
        help_heading = "Filter options",
        help = "Require a missing or null frontmatter field"
    )]
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, clap::Args)]
pub struct ValidateTriageArgs {
    #[arg(
        long,
        help_heading = "Triage filters",
        help = "Filter findings by code. Comma-separated values match any listed code"
    )]
    pub code: Vec<String>,
    #[arg(
        long,
        help_heading = "Triage filters",
        help = "Filter findings by severity"
    )]
    pub severity: Vec<String>,
    #[arg(
        long,
        help_heading = "Triage filters",
        help = "Filter findings by frontmatter field"
    )]
    pub field: Vec<String>,
    #[arg(
        long,
        help_heading = "Triage filters",
        help = "Filter findings by validate rule name"
    )]
    pub rule: Vec<String>,
    #[arg(
        long,
        help_heading = "Triage filters",
        help = "Filter findings by vault-relative path glob using config glob semantics"
    )]
    pub path: Vec<String>,
    #[arg(
        long,
        help_heading = "Triage filters",
        help = "Filter link findings by link target"
    )]
    pub target: Vec<String>,
    #[arg(
        long,
        help_heading = "Triage filters",
        help = "Filter link findings by unresolved reason"
    )]
    pub reason: Vec<String>,
}

#[derive(Debug, Parser)]
pub struct RepairPlanArgs {
    #[arg(long, value_enum, default_value_t = RepairOutputFormat::Json, help = "Stdout format")]
    pub format: RepairOutputFormat,
    #[arg(
        long,
        help = "Write the JSON repair plan artifact to this path instead of stdout"
    )]
    pub out: Option<Utf8PathBuf>,
    #[command(flatten)]
    pub triage: ValidateTriageArgs,
}

#[derive(Debug, Parser)]
pub struct RepairLinksArgs {
    #[arg(
        long,
        help = "Exact vault-relative path or unique document stem to analyze for move/delete risk"
    )]
    pub target: Option<String>,
    #[arg(
        long = "move-to",
        help = "Hypothetical destination path; computes link risk + warnings as if the target were moved to this path"
    )]
    pub move_to: Option<Utf8PathBuf>,
    #[arg(long, value_enum, default_value_t = RepairOutputFormat::Json, help = "Stdout format")]
    pub format: RepairOutputFormat,
}

#[derive(Debug, Parser)]
pub struct RepairApplyArgs {
    #[arg(help = "Path to a JSON repair plan artifact produced by `vault repair plan --out`")]
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
    #[command(flatten)]
    pub filters: FrontmatterFilterArgs,
    #[arg(long, value_enum, help = "Stdout format")]
    pub format: Option<OutputFormat>,
}

#[derive(Args, Debug)]
pub struct FindArgs {
    // ── Predicate operators ─────────────────────────────────────────────
    /// Full-text body substring. Case-insensitive. Empty string is a no-op.
    #[arg(long, value_name = "NEEDLE")]
    pub text: Option<String>,

    /// Frontmatter equality predicate `field:value`. JSON-typed (e.g.
    /// `--eq published:true` for bool, `--eq priority:5` for number).
    /// Repeat for multiple predicates; ALL-of across repeats.
    #[arg(long = "eq", value_name = "FIELD:VALUE")]
    pub eq: Vec<String>,

    /// Frontmatter `field` is NOT equal to `value`. Negation of `--eq`.
    /// For array-shaped fields, matches when no element equals the value.
    #[arg(long = "not-eq", value_name = "FIELD:VALUE")]
    pub not_eq: Vec<String>,

    /// Frontmatter `field` is one of the comma-separated values (ANY-of).
    /// E.g. `--in status:backlog,active`. Repeat for multiple fields;
    /// ALL-of across repeats.
    #[arg(long = "in", value_name = "FIELD:V1,V2,...")]
    pub r#in: Vec<String>,

    /// Frontmatter `field` is NOT one of the comma-separated values.
    #[arg(long = "not-in", value_name = "FIELD:V1,V2,...")]
    pub not_in: Vec<String>,

    /// Frontmatter `field` is present (non-null). Repeat for multiple fields.
    #[arg(long = "has", value_name = "FIELD")]
    pub has: Vec<String>,

    /// Frontmatter `field` is absent or null. Repeat for multiple fields.
    #[arg(long = "missing", value_name = "FIELD")]
    pub missing: Vec<String>,

    /// Frontmatter `field` (a date) is before `DATE`. ISO 8601 expected.
    /// E.g. `--before created:2026-05-01`.
    #[arg(long = "before", value_name = "FIELD:DATE")]
    pub before: Vec<String>,

    /// Frontmatter `field` (a date) is after `DATE`.
    #[arg(long = "after", value_name = "FIELD:DATE")]
    pub after: Vec<String>,

    /// Frontmatter `field` (a date) is exactly `DATE`. Accepts `today`.
    #[arg(long = "on", value_name = "FIELD:DATE")]
    pub on: Vec<String>,

    /// Path glob pattern. Repeat for multiple patterns (ANY-of).
    #[arg(long = "path", value_name = "GLOB")]
    pub path: Vec<String>,

    /// Return every document — escape hatch when no predicate is specified.
    /// Without --all and without any predicate, `vault find` prints its help
    /// page (a full-vault dump is almost always a mistake; require opt-in).
    #[arg(long)]
    pub all: bool,

    // ── Sort / limit / paging ───────────────────────────────────────────
    /// Sort by field (frontmatter key, `path`, or `stem`). Ascending by default.
    #[arg(long, value_name = "FIELD")]
    pub sort: Option<String>,

    /// Sort descending (only meaningful with --sort).
    #[arg(long)]
    pub desc: bool,

    /// Maximum number of matches to return. Default 10.
    #[arg(
        long,
        value_name = "N",
        default_value = "10",
        conflicts_with = "no_limit"
    )]
    pub limit: usize,

    /// Return all matches; no limit. Overrides --limit.
    #[arg(long = "no-limit")]
    pub no_limit: bool,

    /// 1-indexed starting offset for paging. Default 1.
    #[arg(long = "starts-at", value_name = "N", default_value = "1")]
    pub starts_at: usize,

    // ── Output ───────────────────────────────────────────────────────────
    /// Output format. Default auto-detects: TTY → records, piped → paths.
    #[arg(long, value_enum)]
    pub format: Option<FindFormat>,

    /// Comma-separated list of frontmatter fields to include in output.
    /// Default: all (records/json/jsonl). Ignored with warning on paths format.
    #[arg(long, value_name = "FIELD1,FIELD2,...", value_delimiter = ',')]
    pub col: Vec<String>,

    /// Skip the pager even when stdout is a TTY.
    #[arg(long = "no-pager")]
    pub no_pager: bool,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindFormat {
    Paths,
    Records,
    Json,
    Jsonl,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFormat {
    Records,
    Json,
    Jsonl,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorWhen {
    Always,
    Auto,
    Never,
}

#[derive(Debug, Parser)]
pub struct CompletionsCommand {
    #[command(subcommand)]
    pub command: CompletionsSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum CompletionsSubcommand {
    #[command(
        about = "Emit a shell completion script to stdout",
        long_about = "Emit a shell completion script to stdout.\n\nMeant to be sourced or eval'd by the user's shell at startup. For one-command setup, prefer `vault completions install [shell]`."
    )]
    Init(CompletionsInitArgs),
    #[command(
        about = "Install completions into the user's shell config",
        long_about = "Install completions into the user's shell config.\n\nAuto-detects the target shell from $SHELL if no argument is given. Idempotent via a marker comment block; pass --force to overwrite an existing install. Pass --print to preview without writing."
    )]
    Install(CompletionsInstallArgs),
}

#[derive(Debug, Parser)]
pub struct CompletionsInitArgs {
    #[arg(value_enum, help = "Target shell")]
    pub shell: SupportedShell,
}

#[derive(Debug, Parser)]
pub struct CompletionsInstallArgs {
    #[arg(
        value_enum,
        help = "Target shell. Auto-detected from $SHELL if omitted"
    )]
    pub shell: Option<SupportedShell>,
    #[arg(long, help = "Preview what would be written; do not modify any files")]
    pub print: bool,
    #[arg(long, help = "Overwrite an existing install marker block")]
    pub force: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SupportedShell {
    Bash,
    Zsh,
    Fish,
    Powershell,
    Elvish,
    Nushell,
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
    #[command(flatten)]
    pub triage: ValidateTriageArgs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Json,
    Jsonl,
    Table,
    Paths,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
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

#[derive(Debug, Parser)]
pub struct ConfigCommand {
    #[command(subcommand)]
    pub command: ConfigSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum ConfigSubcommand {
    #[command(about = "Show effective config: paths + counts")]
    Show(ConfigShowArgs),
    #[command(about = "Validate the config file itself")]
    Validate(ConfigValidateArgs),
    #[command(about = "Migrate the config file to the current schema version")]
    Migrate,
    #[command(about = "Open the config file in $VISUAL or $EDITOR")]
    Edit(ConfigEditArgs),
}

#[derive(Debug, Args)]
pub struct ConfigShowArgs {
    #[arg(long, value_enum, help = "Stdout format")]
    pub format: Option<ConfigFormat>,
    #[arg(long = "no-pager", help = "Bypass the pager even on TTY records")]
    pub no_pager: bool,
}

#[derive(Debug, Args)]
pub struct ConfigValidateArgs {
    #[arg(long, value_enum, help = "Stdout format")]
    pub format: Option<ConfigFormat>,
}

#[derive(Debug, Args)]
pub struct ConfigEditArgs {
    #[arg(
        long = "no-validate",
        help = "Skip auto-validation after the editor exits"
    )]
    pub no_validate: bool,
}

#[derive(Debug, Args)]
pub struct InitArgs {
    #[arg(long, help = "Overwrite an existing .vault/config.yaml")]
    pub force: bool,
}
