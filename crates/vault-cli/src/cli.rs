use camino::Utf8PathBuf;
use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "vault")]
#[command(about = "Deterministic Markdown vault graph tools")]
#[command(version)]
#[command(disable_help_flag = true)]
#[command(disable_help_subcommand = true)]
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
        help = "Skip the implicit cache refresh before reading the graph"
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
    #[arg(
        short = 'h',
        global = true,
        help_heading = "Global options",
        help = "Print short help. Use --help for full help",
        action = clap::ArgAction::SetTrue,
    )]
    pub help_short: bool,
    #[arg(
        long = "help",
        global = true,
        help_heading = "Global options",
        help = "Print full help. Use -h for a short summary",
        action = clap::ArgAction::SetTrue,
    )]
    pub help_long: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(
        disable_help_flag = true,
        about = "Emit inventoried vault files",
        long_about = "Emit inventoried vault files.\n\nFiles include Markdown documents and non-Markdown attachments. File records can be used with exact-path backlink queries for resolved attachment targets."
    )]
    Files(GraphArgs),
    #[command(
        disable_help_flag = true,
        about = "Find documents in the vault — full-text + metadata filters with sort/limit/paging"
    )]
    Find(FindArgs),
    #[command(
        disable_help_flag = true,
        about = "Count documents in the vault — grouped or total — with the find filter surface"
    )]
    Count(CountArgs),
    #[command(
        disable_help_flag = true,
        about = "Show one or more documents — frontmatter, headings, outgoing/incoming/unresolved links",
        long_about = "Show one or more documents in detail.\n\nEach target may be a vault-relative path, a unique case-insensitive document stem, or a wikilink-shaped string (with or without brackets, with or without anchor / block-ref / pipe-alias suffix). Ambiguous targets emit one record per resolved candidate. --body adds full body content; --col narrows the default field set."
    )]
    Show(ShowArgs),
    #[command(disable_help_flag = true, about = "Scaffold .vault/config.yaml")]
    Init(InitArgs),
    #[command(
        disable_help_flag = true,
        about = "Plan and apply deterministic vault repairs"
    )]
    Repair(RepairCommand),
    #[command(
        disable_help_flag = true,
        about = "Validate vault graph facts and configured frontmatter rules",
        long_about = "Validate vault graph facts and configured frontmatter rules.\n\nValidation reuses graph/index facts to surface unresolved links, ambiguous links, document diagnostics, and configured frontmatter requirements. Validate does not mutate files."
    )]
    Validate(ValidateArgs),
    #[command(
        disable_help_flag = true,
        about = "Shell completion installation and script emission"
    )]
    Completions(CompletionsCommand),
    #[command(
        disable_help_flag = true,
        about = "Manage the SQLite-backed vault graph cache",
        long_about = "Manage the SQLite-backed vault graph cache.\n\nThe cache is a per-vault disposable read-acceleration store. Query commands open it transparently; these subcommands let you index, rebuild, clear, or inspect it explicitly."
    )]
    Cache(CacheCommand),
    #[command(
        disable_help_flag = true,
        about = "Manage the per-vault `.vault/config.yaml`"
    )]
    Config(ConfigCommand),
    #[command(
        hide = true,
        disable_help_flag = true,
        about = "Emit roff-format man page to stdout"
    )]
    Manpage,
}

#[derive(Debug, Parser)]
#[command(disable_help_flag = true)]
pub struct CacheCommand {
    #[command(subcommand)]
    pub command: CacheSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum CacheSubcommand {
    #[command(
        disable_help_flag = true,
        about = "Update the cache incrementally",
        long_about = "Update the cache incrementally.\n\nDetects changed files via mtime+size and re-parses only the affected documents. Pass --rebuild to force a full from-scratch rebuild, or --force-hash to bypass the cheap-check and hash every file."
    )]
    Index(CacheIndexArgs),
    #[command(disable_help_flag = true, about = "Rebuild the cache from scratch")]
    Rebuild,
    #[command(
        disable_help_flag = true,
        about = "Delete the cache database",
        long_about = "Delete the cache database.\n\nRemoves the cache.db file and its WAL/SHM siblings. The next cache-aware command auto-recreates a fresh database."
    )]
    Clear,
    #[command(
        disable_help_flag = true,
        about = "Show cache path, size, document and link counts, and schema version"
    )]
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
#[command(disable_help_flag = true)]
pub struct RepairCommand {
    #[command(subcommand)]
    pub command: RepairSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum RepairSubcommand {
    #[command(
        disable_help_flag = true,
        about = "Generate an explicit repair plan from validation findings",
        long_about = "Generate an explicit repair plan from validation findings.\n\nRepair planning is read-only. It uses configured deterministic repair rules to produce applyable changes, and reports skipped, unsupported, and ambiguous findings as non-blocking planning fallout."
    )]
    Plan(RepairPlanArgs),
    #[command(
        disable_help_flag = true,
        about = "Report link and path repair risks without writing files",
        long_about = "Report link and path repair risks without writing files.\n\nThis surfaces unresolved links, ambiguous links, duplicate-stem risks, path-style Markdown links, affected files, and optional move/delete risk for a target."
    )]
    Links(RepairLinksArgs),
    #[command(
        disable_help_flag = true,
        about = "Apply a repair plan: mutate frontmatter and rewrite broken wikilinks per the plan.",
        long_about = "Apply a repair plan: mutate frontmatter and rewrite broken wikilinks per the plan.\n\nReads a JSON plan emitted by `vault repair plan` and applies each change. Mutates frontmatter (`set_frontmatter` / `remove_frontmatter` / `add_frontmatter` / `move_document`) and source-doc wikilinks (`rewrite_link`). Preserves the Markdown body except for `rewrite_link`, which updates matching wikilinks while preserving display text, anchor (`#section`), and block-ref (`^block-id`) suffixes. Plan changes are gated by precondition checks: the source-doc hash must match what the plan recorded, and `expected_old_value` must match the current frontmatter value. Apply writes by default; pass `--dry-run` to preview. Rejects unsupported schemas, stale hashes, expected-old-value mismatches, conflicting changes, and unsupported operations."
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
    /// Filter closest-match proposals by confidence band.
    /// Default: emit all bands. `high` drops Medium proposals (and their footnotes).
    #[arg(long, value_enum)]
    pub confidence: Option<ConfidenceArg>,
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

/// Shared filter-predicate flags used by `vault find` (and, soon, `vault count`).
///
/// Kept in `cli.rs` so the build script (`build.rs`) can include this file
/// without intra-crate deps — `FilterArgs` only derives `clap::Args`.
/// The translation logic (`build_document_query`) lives in `filter_args.rs`.
#[derive(Args, Debug, Default)]
pub struct FilterArgs {
    /// Full-text body substring. Case-insensitive. Empty string is a no-op.
    #[arg(long, value_name = "NEEDLE", help_heading = "Filter options")]
    pub text: Option<String>,

    /// Frontmatter equality predicate `field:value`. JSON-typed.
    #[arg(
        long = "eq",
        value_name = "FIELD:VALUE",
        help_heading = "Filter options"
    )]
    pub eq: Vec<String>,

    /// Frontmatter `field` is NOT equal to `value`.
    #[arg(
        long = "not-eq",
        value_name = "FIELD:VALUE",
        help_heading = "Filter options"
    )]
    pub not_eq: Vec<String>,

    /// Frontmatter `field` is one of the comma-separated values (ANY-of).
    #[arg(
        long = "in",
        value_name = "FIELD:V1,V2,...",
        help_heading = "Filter options"
    )]
    pub r#in: Vec<String>,

    /// Frontmatter `field` is NOT one of the comma-separated values.
    #[arg(
        long = "not-in",
        value_name = "FIELD:V1,V2,...",
        help_heading = "Filter options"
    )]
    pub not_in: Vec<String>,

    /// Frontmatter `field` is present (non-null).
    #[arg(long = "has", value_name = "FIELD", help_heading = "Filter options")]
    pub has: Vec<String>,

    /// Frontmatter `field` is absent or null.
    #[arg(
        long = "missing",
        value_name = "FIELD",
        help_heading = "Filter options"
    )]
    pub missing: Vec<String>,

    /// Frontmatter `field` (a date) is before `DATE`. ISO 8601 expected.
    #[arg(
        long = "before",
        value_name = "FIELD:DATE",
        help_heading = "Filter options"
    )]
    pub before: Vec<String>,

    /// Frontmatter `field` (a date) is after `DATE`.
    #[arg(
        long = "after",
        value_name = "FIELD:DATE",
        help_heading = "Filter options"
    )]
    pub after: Vec<String>,

    /// Frontmatter `field` (a date) is exactly `DATE`. Accepts `today`.
    #[arg(
        long = "on",
        value_name = "FIELD:DATE",
        help_heading = "Filter options"
    )]
    pub on: Vec<String>,

    /// Path glob pattern.
    #[arg(long = "path", value_name = "GLOB", help_heading = "Filter options")]
    pub path: Vec<String>,
}

#[derive(Args, Debug)]
pub struct FindArgs {
    // ── Filter predicates ──────────────────────────────────────────────
    #[command(flatten)]
    pub filters: FilterArgs,

    /// Return every document — escape hatch when no predicate is specified.
    /// Without --all and without any predicate, `vault find` prints its help
    /// page (a full-vault dump is almost always a mistake; require opt-in).
    #[arg(long, help_heading = "Filter options")]
    pub all: bool,

    // ── Sort / limit / paging ───────────────────────────────────────────
    /// Sort by field (frontmatter key, `path`, or `stem`). Ascending by default.
    #[arg(long, value_name = "FIELD", help_heading = "Sort and paging")]
    pub sort: Option<String>,

    /// Sort descending (only meaningful with --sort).
    #[arg(long, help_heading = "Sort and paging")]
    pub desc: bool,

    /// Maximum number of matches to return. Default 10.
    #[arg(
        long,
        value_name = "N",
        default_value = "10",
        conflicts_with = "no_limit",
        help_heading = "Sort and paging"
    )]
    pub limit: usize,

    /// Return all matches; no limit. Overrides --limit.
    #[arg(long = "no-limit", help_heading = "Sort and paging")]
    pub no_limit: bool,

    /// 1-indexed starting offset for paging. Default 1.
    #[arg(
        long = "starts-at",
        value_name = "N",
        default_value = "1",
        help_heading = "Sort and paging"
    )]
    pub starts_at: usize,

    // ── Output ───────────────────────────────────────────────────────────
    /// Output format. Default auto-detects: TTY → records, piped → paths.
    #[arg(long, value_enum, help_heading = "Output")]
    pub format: Option<FindFormat>,

    /// Comma-separated list of frontmatter fields to include in output.
    /// Default: all (records/json/jsonl). Ignored with warning on paths format.
    #[arg(
        long,
        value_name = "FIELD1,FIELD2,...",
        value_delimiter = ',',
        help_heading = "Output"
    )]
    pub col: Vec<String>,

    /// Skip the pager even when stdout is a TTY.
    #[arg(long = "no-pager", help_heading = "Output")]
    pub no_pager: bool,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindFormat {
    Paths,
    Records,
    Json,
    Jsonl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ValidateFormat {
    /// Human-legible records (TTY default). Summary or per-finding blocks
    /// composed from output::primitives.
    Records,
    /// One JSON object per finding, streaming.
    Jsonl,
    /// Single JSON object wrapper with a `findings` array.
    Json,
    /// One path per affected document, sorted and deduped.
    Paths,
}

#[derive(Args, Debug)]
pub struct CountArgs {
    /// Frontmatter field to group document counts by. Without --by,
    /// emits only the total.
    #[arg(long = "by", value_name = "FIELD", help_heading = "Count options")]
    pub by: Option<String>,

    #[command(flatten)]
    pub filters: FilterArgs,

    /// Output format. Default text (records-block).
    #[arg(long, value_enum, default_value_t = CountFormat::Text, help_heading = "Output")]
    pub format: CountFormat,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountFormat {
    Text,
    Json,
}

#[derive(Args, Debug)]
pub struct ShowArgs {
    /// One or more doc targets. Each accepts path, stem, or wikilink-shaped
    /// input (with or without [[]]). Anchor / block-ref / pipe-alias
    /// suffixes are stripped before resolution.
    #[arg(required = true, num_args = 1.., value_name = "DOC")]
    pub targets: Vec<String>,

    /// Include full body content in each record.
    #[arg(long, help_heading = "Output")]
    pub body: bool,

    /// Comma-separated list of fields to include. Subtractive narrowing.
    /// Without --col, every default field is emitted. Accepts: path,
    /// frontmatter, headings, outgoing_links, unresolved_links,
    /// incoming_links, body (the last only meaningful with --body).
    #[arg(
        long,
        value_name = "FIELD1,FIELD2,...",
        value_delimiter = ',',
        help_heading = "Output"
    )]
    pub col: Vec<String>,

    /// Output format. Default text (records-block per doc).
    #[arg(long, value_enum, default_value_t = ShowFormat::Text, help_heading = "Output")]
    pub format: ShowFormat,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShowFormat {
    Text,
    Json,
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
#[command(disable_help_flag = true)]
pub struct CompletionsCommand {
    #[command(subcommand)]
    pub command: CompletionsSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum CompletionsSubcommand {
    #[command(
        disable_help_flag = true,
        about = "Emit a shell completion script to stdout",
        long_about = "Emit a shell completion script to stdout.\n\nMeant to be sourced or eval'd by the user's shell at startup. For one-command setup, prefer `vault completions install [shell]`."
    )]
    Init(CompletionsInitArgs),
    #[command(
        disable_help_flag = true,
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
    pub format: Option<ValidateFormat>,
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

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
#[clap(rename_all = "snake_case")]
pub enum ConfidenceArg {
    High,
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
#[command(disable_help_flag = true)]
pub struct ConfigCommand {
    #[command(subcommand)]
    pub command: ConfigSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum ConfigSubcommand {
    #[command(
        disable_help_flag = true,
        about = "Show effective config: paths + counts"
    )]
    Show(ConfigShowArgs),
    #[command(disable_help_flag = true, about = "Validate the config file itself")]
    Validate(ConfigValidateArgs),
    #[command(
        disable_help_flag = true,
        about = "Migrate the config file to the current schema version"
    )]
    Migrate,
    #[command(
        disable_help_flag = true,
        about = "Open the config file in $VISUAL or $EDITOR"
    )]
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

#[cfg(test)]
mod count_cli_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn count_parses_with_by_flag() {
        let cli = Cli::try_parse_from(["vault", "count", "--by", "status"]).unwrap();
        match cli.command {
            Command::Count(args) => {
                assert_eq!(args.by.as_deref(), Some("status"));
            }
            _ => panic!("expected Count variant"),
        }
    }

    #[test]
    fn count_parses_without_by() {
        let cli = Cli::try_parse_from(["vault", "count"]).unwrap();
        assert!(matches!(cli.command, Command::Count(_)));
    }

    #[test]
    fn count_inherits_filter_flags() {
        let cli =
            Cli::try_parse_from(["vault", "count", "--eq", "type:note", "--by", "status"]).unwrap();
        match cli.command {
            Command::Count(args) => {
                assert_eq!(args.filters.eq, vec!["type:note".to_string()]);
                assert_eq!(args.by.as_deref(), Some("status"));
            }
            _ => panic!("expected Count variant"),
        }
    }
}

#[cfg(test)]
mod show_cli_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn show_requires_at_least_one_target() {
        assert!(Cli::try_parse_from(["vault", "show"]).is_err());
    }

    #[test]
    fn show_parses_single_target() {
        let cli = Cli::try_parse_from(["vault", "show", "Notes.md"]).unwrap();
        match cli.command {
            Command::Show(args) => assert_eq!(args.targets, vec!["Notes.md".to_string()]),
            _ => panic!("expected Show variant"),
        }
    }

    #[test]
    fn show_parses_multiple_targets() {
        let cli = Cli::try_parse_from(["vault", "show", "a.md", "b.md", "c.md"]).unwrap();
        match cli.command {
            Command::Show(args) => assert_eq!(args.targets.len(), 3),
            _ => panic!("expected Show variant"),
        }
    }

    #[test]
    fn show_parses_body_flag() {
        let cli = Cli::try_parse_from(["vault", "show", "a.md", "--body"]).unwrap();
        match cli.command {
            Command::Show(args) => assert!(args.body),
            _ => panic!("expected Show variant"),
        }
    }

    #[test]
    fn show_parses_col_narrowing() {
        let cli =
            Cli::try_parse_from(["vault", "show", "a.md", "--col", "incoming_links"]).unwrap();
        match cli.command {
            Command::Show(args) => {
                assert_eq!(args.col, vec!["incoming_links".to_string()]);
            }
            _ => panic!("expected Show variant"),
        }
    }

    #[test]
    fn show_format_defaults_text() {
        let cli = Cli::try_parse_from(["vault", "show", "a.md"]).unwrap();
        match cli.command {
            Command::Show(args) => assert_eq!(args.format, ShowFormat::Text),
            _ => panic!("expected Show variant"),
        }
    }
}
