use camino::Utf8PathBuf;
use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "norn")]
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
        help = "Run as if norn started in this directory (default: $NORN_ROOT, else the current directory)"
    )]
    pub cwd: Option<Utf8PathBuf>,
    #[arg(
        long,
        global = true,
        help_heading = "Global options",
        help = "YAML config file. Defaults to <cwd>/.norn/config.yaml when present"
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
        about = "Find documents in the vault — full-text + metadata filters with sort/limit/paging"
    )]
    Find(FindArgs),
    #[command(
        disable_help_flag = true,
        about = "Count documents in the vault — grouped or total — with the find filter surface"
    )]
    Count(CountArgs),
    #[command(
        about = "Describe the vault: structure (placement) and, with --data, a contents-summary"
    )]
    Describe(DescribeArgs),
    #[command(
        disable_help_flag = true,
        about = "Get one or more documents — frontmatter, headings, outgoing/incoming/unresolved links",
        long_about = "Get one or more documents in detail.\n\nEach target may be a vault-relative path, a unique case-insensitive document stem, or a wikilink-shaped string (with or without brackets, with or without anchor / block-ref / pipe-alias suffix). Ambiguous targets emit one record per resolved candidate. --all-cols adds the full structured dump (incl. body); --col narrows the default field set. Supports --sort/--limit/--starts-at over the named targets."
    )]
    Get(GetArgs),
    #[command(
        disable_help_flag = true,
        about = "Update one document — schema-aware frontmatter mutation + wholesale body replacement",
        long_about = "Update one document: mutate frontmatter fields and optionally replace the body.\n\
\n\
WORKFLOW: norn set runs schema-aware validation against the configured field_types rules, \
then applies all ops as a single atomic filesystem write, then emits a SetReport (records on \
TTY, JSON when piped or --format json is set).\n\
\n\
SAFE BY DEFAULT: norn set is destructive. In a TTY, it shows a preview and prompts for \
confirmation. Without --yes (and in a non-TTY context), nothing is written — the preview is \
your dry-run.\n\
\n\
Safety flags:\n  \
--yes            Skip the confirmation prompt and apply.\n  \
--dry-run        Show the preview and exit without writing or prompting.\n  \
--force          Bypass schema enforcement (type validation + required-field protection).\n  \
--format records|json  Output shape. --format json is non-interactive and emits the SetReport envelope.\n\
\n\
Flag classes:\n  \
--field KEY=VALUE      Set a frontmatter field. Multi-instance of the same key accumulates into an array.\n  \
--field-json KEY=JSON  Set a field with an explicit JSON-parsed value (arrays, objects, null).\n  \
--push KEY=VALUE       Append to a list-typed field; creates a single-element array if absent.\n  \
--pop KEY=VALUE        Remove a value from a list-typed field. Silent no-op if absent.\n  \
--remove KEY           Drop a frontmatter key entirely. Silent no-op if the key is missing.\n  \
--body-from-stdin      Read new body content from stdin (wholesale body replacement).\n\
\n\
Exit codes: 0 success or dry-run, 1 operator-cancelled, 2 pre-flight refusal."
    )]
    Set(SetArgs),
    #[command(
        disable_help_flag = true,
        about = "Edit one document's body — atomic content-anchored partial edits",
        long_about = "Edit one document's body with atomic, content-anchored partial edits.\n\
\n\
The edits are an ordered JSON array of ops applied all-or-nothing, each against \
the result of the prior. Supply it via --edits-json or on stdin. For wholesale \
body replacement use `norn set --body-from-stdin`; for frontmatter use `norn set`.\n\
\n\
Ops (tagged by \"op\"):\n  \
str_replace {old,new,replace_all?}   Replace literal text. Unique-match-or-refuse unless replace_all.\n  \
replace_section {heading,content}    Replace a section's body (heading kept).\n  \
append_to_section {heading,content}  Append to a section's body.\n  \
delete_section {heading}             Remove a heading and its body.\n  \
insert_before_heading {heading,content} / insert_after_heading {heading,content}\n\
\n\
Headings are addressed by exact text; a duplicated heading refuses as ambiguous. \
Any anchor failure refuses the whole batch (exit 2) and writes nothing.\n\
\n\
Safety: destructive. In a TTY it previews and prompts; without --yes in a \
non-TTY, nothing is written (preview is your dry-run). --dry-run forces preview.\n\
\n\
Exit codes: 0 success or dry-run, 1 operator-cancelled, 2 pre-flight refusal."
    )]
    Edit(EditArgs),
    #[command(
        disable_help_flag = true,
        about = "Create a new document — schema-aware frontmatter pre-fill from path rules",
        long_about = "Create a new Markdown document with frontmatter pre-filled from the path's schema rules.\n\
\n\
`norn new` is the create verb of the CRUD-ish mutation surface (sibling to `norn get`, `norn set`,\n\
`norn move`, `norn delete`). It infers required-field defaults from matching schema rules, applies\n\
substitution (date/time/title/path variables), and writes the new document atomically.\n\
\n\
Operator overrides via --field always win over schema defaults. Refuses if path exists (unless\n\
--force) or parent directory missing (unless -p). Safe-by-default apply model with TTY confirm,\n\
non-TTY implicit dry-run, --yes, and --dry-run.\n\
\n\
After write, `norn validate` runs against the new doc; findings surface as envelope warnings."
    )]
    New(NewArgs),
    #[command(disable_help_flag = true, about = "Scaffold .norn/config.yaml")]
    Init(InitArgs),
    #[command(
        disable_help_flag = true,
        about = "Move/rename a document with cascading backlink rewrites",
        long_about = "Move or rename a document and rewrite incoming wikilinks across the vault.\n\
\n\
SAFE BY DEFAULT: norn move is destructive. In a TTY, it shows a preview and prompts for confirmation. \
Without --yes (and in a non-TTY context), nothing is written — the preview is your dry-run.\n\
\n\
Flags:\n  \
--yes            Skip the confirmation prompt and apply.\n  \
--dry-run        Show the preview and exit without writing or prompting.\n  \
--force          Overwrite the destination if it already exists (otherwise refused with exit 2).\n  \
--no-link-rewrite  Move the file but do NOT rewrite incoming links (they'll surface as broken).\n  \
--format records|json  Output shape. --format json is non-interactive and emits the ApplyReport envelope.\n\
\n\
Exit codes: 0 success or dry-run, 1 user-cancelled or runtime failure, 2 pre-flight refusal."
    )]
    Move(MoveArgs),
    #[command(
        name = "delete",
        disable_help_flag = true,
        about = "Delete a document, optionally redirecting incoming links to an alternate target",
        long_about = "Delete a document, optionally redirecting incoming links to an alternate target.\n\
\n\
SAFE BY DEFAULT: norn delete is destructive. In a TTY, it shows a preview and prompts for confirmation. \
Without --yes (and in a non-TTY context), nothing is written — the preview is your dry-run.\n\
\n\
Incoming links: norn delete REFUSES (exit 2) when the target has incoming links unless one of these is given:\n  \
--allow-broken-links   Delete and let the broken links surface as link-target-missing findings in norn validate.n  \
--rewrite-to <ALT>     Redirect every incoming link to <ALT> before deleting. Mutually exclusive with --allow-broken-links.\n\
\n\
Flags:\n  \
--yes            Skip the confirmation prompt and apply.\n  \
--dry-run        Show the preview and exit without writing or prompting.\n  \
--format records|json  Output shape. --format json is non-interactive and emits the ApplyReport envelope.\n\
\n\
Exit codes: 0 success or dry-run, 1 user-cancelled or runtime failure, 2 pre-flight refusal."
    )]
    Delete(DeleteArgs),
    #[command(
        disable_help_flag = true,
        about = "Apply a MigrationPlan — move, delete, rewrite, and frontmatter ops from a plan file"
    )]
    Migrate(MigrateArgs),
    #[command(
        disable_help_flag = true,
        about = "Surface deterministic-repair findings; --plan emits a MigrationPlan",
        long_about = "Surface deterministic-repair findings for the vault.\n\nBare `norn repair` prints a read-only findings summary (placeholder for a future interactive workflow).\n\n`norn repair --plan` generates a MigrationPlan from the configured deterministic repair rules and emits it as `report` (human summary, TTY default), `json` (full MigrationPlan envelope, pipe default), or `paths` (one affected path per line). Pipe the JSON into `norn migrate -` to apply it. Use `--skip-reason <PATTERN>` to filter skipped findings by reason code; glob patterns accepted."
    )]
    Repair(RepairArgs),
    #[command(
        name = "rewrite-wikilink",
        disable_help_flag = true,
        about = "Rewrite all occurrences of a wikilink target across the vault (body + frontmatter)",
        long_about = "Rewrite all occurrences of a wikilink target across the vault.\n\
\n\
Rewrites both body wikilinks (`[[OLD]]`, `[[OLD|display]]`) and frontmatter fields\n\
that contain the old target as a wikilink value. Builds a one-op MigrationPlan and\n\
runs through the unified applier.\n\
\n\
Pre-flight refusal (exit 2) when OLD does not resolve to any document.\n\
\n\
Exit codes: 0 success or dry-run, 1 runtime failure, 2 pre-flight refusal."
    )]
    RewriteWikilink(RewriteWikilinkArgs),
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
        about = "Manage the per-vault `.norn/config.yaml`"
    )]
    Config(ConfigCommand),
    #[command(
        hide = true,
        disable_help_flag = true,
        about = "Emit roff-format man page to stdout"
    )]
    Manpage,
    #[command(
        disable_help_flag = true,
        about = "Update norn to the latest GitHub release",
        long_about = "Update norn to the latest GitHub release.\n\n\
            Only works when norn was installed via the official GitHub install \
            script. If you installed via `cargo install`, Homebrew, or built \
            from source, use that tool's update mechanism instead.\n\n\
            `--dry-run` resolves the target version and prints the plan without \
            downloading or modifying anything. Combine with `--format json` for \
            scriptable \"is an update available?\" checks."
    )]
    SelfUpdate(SelfUpdateArgs),
    /// Run norn as a Model Context Protocol (MCP) stdio server over the vault at --cwd.
    Mcp(McpArgs),
    #[command(
        about = "Run the warm host daemon: serves MCP for any vault on this host over a Unix socket",
        long_about = "Run the warm host daemon.\n\n\
            `norn serve` is ONE persistent foreground process (Unix only) that serves the full \
            MCP toolset for any vault on this host over a single well-known Unix-domain socket \
            (`<XDG_CACHE_HOME>/norn/run/norn.sock`). It runs in the foreground — run it under a \
            process supervisor; supervision verbs (`norn service start/stop/status`) arrive later. \
            At most one instance runs per user (guarded by a lifetime advisory lock next to the \
            socket); a second `norn serve` refuses.\n\n\
            Vaults are named per-connection, so `--cwd` is not used for data and `--config` is not \
            accepted (each vault loads its own default `.norn/config.yaml`). Per vault, the daemon \
            holds a verify-once warm cache: integrity is checked the first time a vault is touched, \
            then maintained by a cheap per-request self-heal — so warm queries skip the repeated \
            integrity check the one-shot CLI pays.\n\n\
            Shut down cleanly with SIGINT/SIGTERM (the socket is unlinked on exit)."
    )]
    Serve(ServeArgs),
    #[command(
        disable_help_flag = true,
        about = "Read the vault mutation audit trail (the append-only event stream): recent mutations with status / target / trace, filterable"
    )]
    Audit(AuditArgs),
}

#[derive(Debug, clap::Args)]
pub struct McpArgs {
    /// Expose only read tools; drop all mutation tools from tools/list.
    #[arg(long)]
    pub read_only: bool,
}

/// Arguments for `norn serve`. Phase 1 has no flags: there is no `--detach`,
/// `--read-only`, or `--socket` — the daemon is a single foreground process at
/// the well-known socket, serving the full toolset. Supervision is a later
/// `norn service` layer (NRN-115).
#[derive(Debug, clap::Args)]
pub struct ServeArgs {}

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
    #[command(
        disable_help_flag = true,
        about = "Evict dead, aged, and over-cap cache entries across all vaults",
        long_about = "Evict dead, aged, and over-cap cache entries across all vaults.\n\nScans the global cache tree (~/.cache/norn/) and state tree (~/.local/state/norn/). Cache entries are evicted when their vault root no longer exists, they are unreadable or empty (lock-file-only entries count as empty), they exceed the retention window (default 90d), or the tree exceeds its 1 GiB cap (oldest first). State entries (the mutation event stream) are evicted only when their vault root no longer exists (or the entry is empty). The current vault's entries are never evicted."
    )]
    Prune(CachePruneArgs),
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
pub struct CachePruneArgs {
    #[arg(long, help = "Report what would be evicted without deleting anything")]
    pub dry_run: bool,
    #[arg(
        long,
        value_parser = parse_retention_arg,
        help = "Age-eviction window override (e.g. 90d, 12w, 24h)"
    )]
    pub retention: Option<std::time::Duration>,
    #[arg(long, value_enum, default_value_t = CacheOutputFormat::Text, help = "Stdout format")]
    pub format: CacheOutputFormat,
}

fn parse_retention_arg(s: &str) -> Result<std::time::Duration, String> {
    crate::standards::parse_duration(s)
        .ok_or_else(|| format!("invalid duration '{s}' (expected e.g. 90d, 12w, 24h, 30m)"))
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
pub struct RepairArgs {
    /// Generate a MigrationPlan from current findings (read-only). Without this
    /// flag, `norn repair` prints a findings summary instead.
    #[arg(long)]
    pub plan: bool,
    #[arg(
        long,
        value_parser = parse_repair_plan_format,
        help = "Output format for --plan (default: report when TTY, json when piped)"
    )]
    pub format: Option<RepairPlanFormat>,
    #[arg(
        long,
        help = "Write the JSON MigrationPlan artifact to this path instead of stdout (--plan only)"
    )]
    pub out: Option<Utf8PathBuf>,
    /// Filter closest-match proposals by confidence band.
    /// Default: emit all bands. `high` drops Medium proposals (and their footnotes).
    #[arg(long, value_enum)]
    pub confidence: Option<ConfidenceArg>,
    #[arg(
        long = "skip-reason",
        value_name = "PATTERN",
        help = "Filter skipped findings by reason code; glob patterns accepted (repeatable)"
    )]
    pub skip_reason: Vec<String>,
    #[command(flatten)]
    pub triage: ValidateTriageArgs,
}

#[derive(Debug, Parser)]
pub struct GraphArgs {
    #[arg(long, value_enum, help = "Stdout format")]
    pub format: Option<OutputFormat>,
}

/// Shared filter-predicate flags used by `norn find` (and, soon, `norn count`).
///
/// Kept in `cli.rs` so the build script (`build.rs`) can include this file
/// without intra-crate deps — `FilterArgs` only derives `clap::Args`.
/// The translation logic (`build_document_query`) lives in `filter_args.rs`.
#[derive(Args, Debug, Default, Clone, PartialEq, Eq)]
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

    /// Frontmatter `field` (or any array element) starts with `VALUE`. Case-sensitive.
    #[arg(
        long = "starts-with",
        value_name = "FIELD:VALUE",
        help_heading = "Filter options"
    )]
    pub starts_with: Vec<String>,

    /// Frontmatter `field` (or any array element) ends with `VALUE`. Case-sensitive.
    #[arg(
        long = "ends-with",
        value_name = "FIELD:VALUE",
        help_heading = "Filter options"
    )]
    pub ends_with: Vec<String>,

    /// Frontmatter `field` (or any array element) contains `VALUE`. Case-sensitive.
    #[arg(
        long = "contains",
        value_name = "FIELD:VALUE",
        help_heading = "Filter options"
    )]
    pub contains: Vec<String>,

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

    /// Documents whose outgoing links resolve to TARGET (path, stem, or
    /// `[[wikilink]]`). Repeatable; multiple targets are AND'd. Resolved-only —
    /// TARGET must resolve to an existing document.
    #[arg(
        long = "links-to",
        value_name = "TARGET",
        help_heading = "Filter options"
    )]
    pub links_to: Vec<String>,

    /// Documents with at least one unresolved link.
    #[arg(long = "unresolved-links", help_heading = "Filter options")]
    pub unresolved_links: bool,
}

/// Shared sort / limit / paging flags for the read commands (`find` / `get`),
/// single-sourced so the surface can't drift. The two deliberate behavioral
/// divergences live in each command's consumption, not here: `find` defaults an
/// absent `--limit` to 10 and sorts in SQL collation; `get` returns every named
/// target by default and sorts in-memory by display string.
#[derive(Args, Debug)]
pub struct SortPaginateArgs {
    /// Sort by field (frontmatter key, `path`, or `stem`). Ascending by default.
    #[arg(long, value_name = "FIELD", help_heading = "Sort and paging")]
    pub sort: Option<String>,

    /// Sort descending (only meaningful with --sort).
    #[arg(long, help_heading = "Sort and paging")]
    pub desc: bool,

    /// Maximum number of records to return. `find` defaults to 10; `get`
    /// returns every named target.
    #[arg(
        long,
        value_name = "N",
        conflicts_with = "no_limit",
        help_heading = "Sort and paging"
    )]
    pub limit: Option<usize>,

    /// Return all records; no limit. Overrides --limit.
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
}

#[derive(Args, Debug)]
pub struct FindArgs {
    // ── Filter predicates ──────────────────────────────────────────────
    #[command(flatten)]
    pub filters: FilterArgs,

    /// Return every document — escape hatch when no predicate is specified.
    /// Without --all and without any predicate, `norn find` prints its help
    /// page (a full-vault dump is almost always a mistake; require opt-in).
    #[arg(long, help_heading = "Filter options")]
    pub all: bool,

    // ── Sort / limit / paging (shared with `get`) ───────────────────────
    // find defaults an absent --limit to 10 (applied in find::query) and
    // sorts in SQL collation.
    #[command(flatten)]
    pub paging: SortPaginateArgs,

    // ── Output ───────────────────────────────────────────────────────────
    /// Output format. Default auto-detects: TTY → records, piped → paths.
    #[arg(long, value_enum, help_heading = "Output")]
    pub format: Option<FindFormat>,

    /// Emit the full structured dump for each match: whole frontmatter plus
    /// every cache-served facet (`.headings`, the three link sets, `.body`).
    /// Excludes `.raw` (so a broad query never fans out to N file reads).
    /// Mutually exclusive with `--col`.
    #[arg(long = "all-cols", conflicts_with = "col", help_heading = "Output")]
    pub all_cols: bool,

    /// Comma-separated columns to include. Bare names select frontmatter
    /// fields (e.g. `status,title`), exactly like `norn get`. Structural
    /// facets are dot-prefixed: `.path`, `.stem`, `.frontmatter` (the whole
    /// block), `.headings`, `.outgoing_links`, `.unresolved_links`,
    /// `.incoming_links`, `.body`, `.raw`, `.document_hash` (the content hash
    /// `edit --expected-hash` wants; opt-in only, like `.raw` — never in
    /// `--all-cols`). Default (no --col): frontmatter
    /// only. Ignored with a warning on paths format.
    #[arg(
        long,
        value_name = "COL1,COL2,...",
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
    /// Frontmatter field(s) to group document counts by, comma-separated.
    /// One field emits a flat distribution; several nest in order
    /// (e.g. --by project,lifecycle). Without --by, emits only the total.
    #[arg(
        long = "by",
        value_name = "FIELD1,FIELD2,...",
        value_delimiter = ',',
        help_heading = "Count options"
    )]
    pub by: Vec<String>,

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
pub struct DescribeArgs {
    /// Include the vault contents-summary (totals, field distributions, date bounds).
    #[arg(long, help_heading = "Describe options")]
    pub data: bool,

    /// Alias for --data.
    #[arg(long, help_heading = "Describe options")]
    pub stats: bool,

    /// Explicit frontmatter field(s) to distribute, comma-separated. Bypasses
    /// the automatic identity-skip. Implies --data.
    #[arg(
        long = "by",
        value_name = "FIELD1,FIELD2,...",
        value_delimiter = ',',
        help_heading = "Describe options"
    )]
    pub by: Vec<String>,

    /// Max value-buckets shown per field (default 20; 0 = no cap).
    #[arg(long, value_name = "N", help_heading = "Describe options")]
    pub limit: Option<usize>,

    #[command(flatten)]
    pub filters: FilterArgs,

    /// Output format. Default text.
    #[arg(long, value_enum, help_heading = "Output")]
    pub format: Option<DescribeFormat>,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescribeFormat {
    Text,
    Json,
}

#[derive(Args, Debug)]
pub struct GetArgs {
    /// One or more doc targets. Each accepts path, stem, or wikilink-shaped
    /// input (with or without [[]]). Anchor / block-ref / pipe-alias
    /// suffixes are stripped before resolution.
    #[arg(required = true, num_args = 1.., value_name = "DOC")]
    pub targets: Vec<String>,

    // ── Sort / limit / paging (shared with `find`) ─────────────────────
    // get's default --limit is None → return every named target (truncating
    // a named set would surprise); sort is applied in-memory in show::run.
    #[command(flatten)]
    pub paging: SortPaginateArgs,

    // ── Output ───────────────────────────────────────────────────────────
    /// Emit the full structured dump: every frontmatter field plus every
    /// cache-served facet (`.headings`, the three link sets, `.body`).
    /// Excludes `.raw` (the disk-read representation, always requested by
    /// name). Mutually exclusive with `--col`.
    #[arg(long = "all-cols", conflicts_with = "col", help_heading = "Output")]
    pub all_cols: bool,

    /// Comma-separated columns to include. Bare names select frontmatter
    /// fields (e.g. `status,title`), exactly like `norn find`. Structural
    /// facets are dot-prefixed: `.path`, `.stem`, `.frontmatter` (the whole
    /// block), `.headings`, `.outgoing_links`, `.unresolved_links`,
    /// `.incoming_links`, `.body`, `.raw`, `.document_hash` (the content hash
    /// `edit --expected-hash` wants; opt-in only, like `.raw` — never in
    /// `--all-cols`). Without --col, frontmatter +
    /// headings + links are emitted (body only with --all-cols or `--col .body`).
    #[arg(
        long,
        value_name = "COL1,COL2,...",
        value_delimiter = ',',
        help_heading = "Output"
    )]
    pub col: Vec<String>,

    /// Named section to read, by exact heading text. Repeatable — pass once
    /// per section (`--section "Task Description" --section "Annotations"`).
    /// Each occurrence is one whole heading string, so a heading that itself
    /// contains a comma (`--section "Risks, Open Questions"`) is addressable
    /// verbatim — the same way `edit` takes a heading as one whole string.
    /// Resolved with the same boundary and failure semantics as
    /// `edit --append-to-section` / `--replace-section` (heading line through
    /// the next same-or-higher heading, or EOF) — a section read mirrors a
    /// section write. Orthogonal to `--col`/`--all-cols`; combine freely. A
    /// heading missing or ambiguous in a given document warns on stderr and is
    /// omitted from that document's `sections` (siblings and other documents
    /// are unaffected); if none of the requested headings resolve for a
    /// document, that is a hard failure (nonzero exit) for that target,
    /// mirroring how `get` already treats a target that fails to resolve at
    /// all. Ignored (with a warning) by `--format paths`/`markdown`, like
    /// `--col`.
    #[arg(
        long,
        value_name = "HEADING",
        num_args = 1,
        action = clap::ArgAction::Append,
        help_heading = "Output"
    )]
    pub section: Vec<String>,

    /// Output format. Default records (vertical key-value block per doc).
    #[arg(long, value_enum, default_value_t = GetFormat::Records, help_heading = "Output")]
    pub format: GetFormat,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum GetFormat {
    /// Vertical key-value record block per document.
    Records,
    /// One document path per line (`--col` is ignored).
    Paths,
    /// A single JSON array of record objects.
    Json,
    /// One JSON record object per line.
    Jsonl,
    /// The single selected document, byte-faithful from disk. Errors unless
    /// exactly one document is selected; `--col` is ignored.
    Markdown,
}

#[derive(Args, Debug)]
pub struct AuditArgs {
    /// Only events from this invocation trace id (one mutation command).
    #[arg(long, value_name = "ID", help_heading = "Filters")]
    pub trace: Option<String>,

    /// Only per-action events with this status. Excludes lifecycle/planned
    /// events (which have no status).
    #[arg(long, value_enum, help_heading = "Filters")]
    pub status: Option<AuditStatus>,

    /// Only events touching this vault-relative path (matches a move's source
    /// or destination).
    #[arg(long, value_name = "PATH", help_heading = "Filters")]
    pub target: Option<String>,

    /// Lower time bound. `YYYY-MM-DD` (start of UTC day) or full RFC-3339.
    #[arg(long, value_name = "WHEN", help_heading = "Filters")]
    pub since: Option<String>,

    /// Upper time bound. `YYYY-MM-DD` (end of UTC day) or full RFC-3339.
    #[arg(long, value_name = "WHEN", help_heading = "Filters")]
    pub until: Option<String>,

    /// Maximum number of events to return, newest-first.
    #[arg(
        long,
        value_name = "N",
        default_value_t = 20,
        help_heading = "Sort and paging"
    )]
    pub limit: usize,

    /// Emit the stored OTEL event objects verbatim instead of the flattened
    /// projection. Affects `--format json` only (ignored by `records`).
    #[arg(long, help_heading = "Output")]
    pub raw: bool,

    /// Output format. Default records (vertical key-value block per event).
    #[arg(long, value_enum, default_value_t = AuditFormat::Records, help_heading = "Output")]
    pub format: AuditFormat,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditFormat {
    /// Vertical key-value record block per event.
    Records,
    /// A single JSON array of event objects.
    Json,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditStatus {
    Applied,
    Skipped,
    Failed,
}

impl AuditStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
        }
    }
}

#[derive(Args, Debug)]
pub struct SetArgs {
    /// The doc to mutate. Path, stem, or wikilink-shaped (with or without [[]]).
    #[arg(value_name = "DOC")]
    pub target: String,

    /// Set a frontmatter field. Repeatable; multiple instances of the same key
    /// accumulate into an array. KEY=VALUE.
    #[arg(long = "field", value_name = "KEY=VALUE")]
    pub fields: Vec<String>,

    /// Set a frontmatter field with a JSON-parsed value. Escape hatch for
    /// structured values (arrays, nested objects, explicit null). KEY=JSON.
    #[arg(long = "field-json", value_name = "KEY=JSON")]
    pub field_json: Vec<String>,

    /// Append a value to a list-typed frontmatter field. Creates a single-element
    /// array if the key doesn't exist. KEY=VALUE.
    #[arg(long, value_name = "KEY=VALUE")]
    pub push: Vec<String>,

    /// Remove a value from a list-typed frontmatter field. Silent no-op if value
    /// not present. KEY=VALUE.
    #[arg(long, value_name = "KEY=VALUE")]
    pub pop: Vec<String>,

    /// Remove a frontmatter key entirely. Silent no-op if key not present.
    #[arg(long, value_name = "KEY")]
    pub remove: Vec<String>,

    /// Read new body content from stdin (wholesale body replacement).
    #[arg(long)]
    pub body_from_stdin: bool,

    /// Bypass schema enforcement (type validation + required-field protection).
    #[arg(long)]
    pub force: bool,

    /// Apply the mutation without an interactive confirm prompt.
    #[arg(long)]
    pub yes: bool,

    /// Preview the mutation without writing.
    #[arg(long)]
    pub dry_run: bool,

    /// Output format.
    #[arg(long, value_enum, default_value_t = SetFormat::Records)]
    pub format: SetFormat,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetFormat {
    Records,
    Json,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum EditFormat {
    Records,
    Json,
}

#[derive(Args, Debug)]
pub struct EditArgs {
    /// The doc to edit. Path, stem, or wikilink-shaped (with or without [[]]).
    #[arg(value_name = "DOC")]
    pub target: String,

    /// The edits as a JSON array of ops (e.g.
    /// `[{"op":"str_replace","old":"a","new":"b"}]`). Mutually exclusive with
    /// stdin; if omitted, the array is read from stdin.
    #[arg(long = "edits-json", value_name = "JSON")]
    pub edits_json: Option<String>,

    /// Refuse the edit unless the document's current content hash equals HASH
    /// (blake3 hex of the full file — the `document_hash` plan ops carry).
    /// Opt-in compare-and-swap; absent = read-modify-write.
    #[arg(long = "expected-hash", value_name = "HASH")]
    pub expected_hash: Option<String>,

    /// Apply the edits without an interactive confirm prompt.
    #[arg(long)]
    pub yes: bool,

    /// Preview the edits without writing.
    #[arg(long)]
    pub dry_run: bool,

    /// Output format.
    #[arg(long, value_enum, default_value_t = EditFormat::Records)]
    pub format: EditFormat,
}

#[derive(Args, Debug)]
pub struct NewArgs {
    /// Vault-relative path of the new document (must end in .md).
    /// Optional: omit when using --as or the inbox fallback.
    pub path: Option<camino::Utf8PathBuf>,

    /// Create into a named, creatable rule; derives the path from its target template.
    #[arg(long = "as", value_name = "RULE")]
    pub as_rule: Option<String>,

    /// Document title — drives the filename and is available to templates as {{title}}.
    #[arg(long = "title", value_name = "TEXT")]
    pub title: Option<String>,

    /// Template variable, repeatable. Format: KEY=VALUE. Fills {{var.KEY}} holes.
    #[arg(long = "var", value_name = "KEY=VALUE")]
    pub var: Vec<String>,

    /// Frontmatter field override, repeatable. Format: KEY=VALUE.
    #[arg(long = "field", value_name = "KEY=VALUE")]
    pub field: Vec<String>,

    /// Frontmatter field with raw JSON value, repeatable. Format: KEY=JSON.
    #[arg(long = "field-json", value_name = "KEY=JSON")]
    pub field_json: Vec<String>,

    /// Read body content from stdin.
    #[arg(long = "body-from-stdin")]
    pub body_from_stdin: bool,

    /// Overwrite existing destination and skip schema-aware coercion.
    #[arg(long)]
    pub force: bool,

    /// Auto-create missing parent directories (mkdir -p style).
    #[arg(short = 'p', long = "parents")]
    pub parents: bool,

    /// Mutate without TTY confirmation.
    #[arg(long)]
    pub yes: bool,

    /// Preview only; never write.
    #[arg(long = "dry-run")]
    pub dry_run: bool,

    /// Output format.
    #[arg(long, value_enum, default_value_t = NewFormat::Records)]
    pub format: NewFormat,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewFormat {
    Records,
    Json,
}

#[derive(Debug, clap::Args)]
pub struct MoveArgs {
    /// Source: vault-relative path or unique stem.
    pub src: String,

    /// Destination: vault-relative path.
    pub dst: String,

    /// Skip interactive confirm and apply.
    #[arg(long)]
    pub yes: bool,

    /// Print summary, exit. No write, no confirm.
    #[arg(long)]
    pub dry_run: bool,

    /// Move the file but skip backlink rewrites.
    #[arg(long)]
    pub no_link_rewrite: bool,

    /// Overwrite destination if it exists.
    #[arg(long)]
    pub force: bool,

    /// Create missing destination parent directories before moving.
    #[arg(long, short = 'p')]
    pub parents: bool,

    /// When SRC and DST are directories, recursively move all .md files
    /// preserving structure (one cascade pass for all backlinks).
    #[arg(long, short = 'r')]
    pub recursive: bool,

    /// Stdout format. `records` is the default TTY summary; `json` emits the ApplyReport.
    #[arg(long, value_enum, default_value_t = MoveFormat::Records)]
    pub format: MoveFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum MoveFormat {
    Records,
    Json,
}

#[derive(Debug, clap::Args)]
pub struct MigrateArgs {
    /// Path to MigrationPlan file (YAML or JSON). Use `-` for stdin.
    #[arg(value_name = "PLAN")]
    pub plan_path: String,

    /// Preview without mutating. Exit code 0, dry_run=true in JSON report.
    #[arg(long)]
    pub dry_run: bool,

    /// Skip TTY confirmation prompt and apply immediately.
    #[arg(long)]
    pub yes: bool,

    /// Output format.
    #[arg(long, value_enum, default_value_t = MigrateFormat::Records)]
    pub format: MigrateFormat,

    /// Input plan format. Auto-detected by extension (.yaml/.yml → YAML, else JSON).
    /// Required for stdin (`-`) when the plan is YAML.
    #[arg(long, value_enum)]
    pub input_format: Option<InputFormat>,

    /// Auto-create missing parent directories for create_document ops
    /// (mkdir -p style). Directories are created only for ops that proceed.
    #[arg(short = 'p', long = "parents")]
    pub parents: bool,

    /// Write the JSON apply report to this file instead of stdout.
    #[arg(long)]
    pub out: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum MigrateFormat {
    Records,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum InputFormat {
    Json,
    Yaml,
}

#[derive(Debug, clap::Args)]
pub struct RewriteWikilinkArgs {
    /// Old wikilink target (stem, path, or alias) to find and rewrite.
    #[arg(value_name = "OLD")]
    pub old: String,

    /// New wikilink target to replace OLD with.
    #[arg(value_name = "NEW")]
    pub new: String,

    /// Preview changes without writing files.
    #[arg(long)]
    pub dry_run: bool,

    /// Skip TTY confirmation prompt and apply immediately.
    #[arg(long)]
    pub yes: bool,

    /// Output format.
    #[arg(long, value_enum, default_value_t = RewriteWikilinkFormat::Records)]
    pub format: RewriteWikilinkFormat,

    /// Write the JSON apply report to this file instead of stdout.
    #[arg(long)]
    pub out: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum RewriteWikilinkFormat {
    Records,
    Json,
}

#[derive(Debug, clap::Args)]
pub struct DeleteArgs {
    /// Document to delete: vault-relative path or unique stem.
    pub doc: String,

    /// Skip interactive confirm and apply.
    #[arg(long)]
    pub yes: bool,

    /// Print summary, exit. No write, no confirm.
    #[arg(long)]
    pub dry_run: bool,

    /// Acknowledge that incoming links will break. Required if the doc has incoming
    /// links and --rewrite-to is not provided.
    #[arg(long, conflicts_with = "rewrite_to")]
    pub allow_broken_links: bool,

    /// Rewrite incoming links to this alternate doc instead of leaving them broken.
    #[arg(long, value_name = "ALT_DOC")]
    pub rewrite_to: Option<String>,

    /// Stdout format. `records` is the default TTY summary; `json` emits the ApplyReport.
    #[arg(long, value_enum, default_value_t = DeleteFormat::Records)]
    pub format: DeleteFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum DeleteFormat {
    Records,
    Json,
}

#[derive(Debug, clap::Args)]
pub struct SelfUpdateArgs {
    /// Install this specific version (e.g. `0.30.0`). Downgrades allowed.
    /// Defaults to the latest GitHub release.
    #[arg(long = "version", id = "pin_version", value_name = "X.Y.Z")]
    pub version: Option<String>,

    /// Resolve the target and print the plan, do not download or modify
    /// anything. Combine with `--format json` for scriptable "is an update
    /// available?" checks.
    #[arg(long)]
    pub dry_run: bool,

    /// Output format. Default: `text` on TTY, `json` when piped.
    #[arg(long, value_enum, help_heading = "Output")]
    pub format: Option<SelfUpdateFormat>,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfUpdateFormat {
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
        long_about = "Emit a shell completion script to stdout.\n\nMeant to be sourced or eval'd by the user's shell at startup. For one-command setup, prefer `norn completions install [shell]`."
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairPlanFormat {
    /// Decision-support report for human review. Default for TTY.
    Report,
    /// Full JSON envelope. Default when piped. Feed to `norn migrate -` to apply.
    Json,
    /// Affected document paths, one per line, sorted and deduplicated.
    Paths,
}

fn parse_repair_plan_format(s: &str) -> Result<RepairPlanFormat, String> {
    // Returns the suffix only — clap wraps with "invalid value '<v>' for '--format <FORMAT>': ".
    match s {
        "report" => Ok(RepairPlanFormat::Report),
        "json" => Ok(RepairPlanFormat::Json),
        "paths" => Ok(RepairPlanFormat::Paths),
        "jsonl" => Err("jsonl was removed — use --format json".into()),
        "table" => Err("table was removed — use --format report".into()),
        _ => Err("possible values: report, json, paths".into()),
    }
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
#[clap(rename_all = "snake_case")]
pub enum ConfidenceArg {
    High,
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
    #[arg(long, help = "Overwrite an existing .norn/config.yaml")]
    pub force: bool,
}

#[cfg(test)]
mod count_cli_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn count_parses_with_by_flag() {
        let cli = Cli::try_parse_from(["norn", "count", "--by", "status"]).unwrap();
        match cli.command {
            Command::Count(args) => {
                assert_eq!(args.by, vec!["status".to_string()]);
            }
            _ => panic!("expected Count variant"),
        }
    }

    #[test]
    fn count_parses_without_by() {
        let cli = Cli::try_parse_from(["norn", "count"]).unwrap();
        assert!(matches!(cli.command, Command::Count(_)));
    }

    #[test]
    fn count_inherits_filter_flags() {
        let cli =
            Cli::try_parse_from(["norn", "count", "--eq", "type:note", "--by", "status"]).unwrap();
        match cli.command {
            Command::Count(args) => {
                assert_eq!(args.filters.eq, vec!["type:note".to_string()]);
                assert_eq!(args.by, vec!["status".to_string()]);
            }
            _ => panic!("expected Count variant"),
        }
    }
}

#[cfg(test)]
mod get_cli_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn get_requires_at_least_one_target() {
        assert!(Cli::try_parse_from(["norn", "get"]).is_err());
    }

    #[test]
    fn get_parses_single_target() {
        let cli = Cli::try_parse_from(["norn", "get", "Notes.md"]).unwrap();
        match cli.command {
            Command::Get(args) => assert_eq!(args.targets, vec!["Notes.md".to_string()]),
            _ => panic!("expected Get variant"),
        }
    }

    #[test]
    fn get_parses_multiple_targets() {
        let cli = Cli::try_parse_from(["norn", "get", "a.md", "b.md", "c.md"]).unwrap();
        match cli.command {
            Command::Get(args) => assert_eq!(args.targets.len(), 3),
            _ => panic!("expected Get variant"),
        }
    }

    #[test]
    fn get_body_flag_is_removed() {
        // Breaking change: `--body` no longer exists. Body now comes via
        // `--all-cols` (full dump) or `--col .body` (body only).
        let res = Cli::try_parse_from(["norn", "get", "a.md", "--body"]);
        assert!(res.is_err(), "expected --body to be an unknown flag");
    }

    #[test]
    fn get_parses_all_cols_flag() {
        let cli = Cli::try_parse_from(["norn", "get", "a.md", "--all-cols"]).unwrap();
        match cli.command {
            Command::Get(args) => assert!(args.all_cols),
            _ => panic!("expected Get variant"),
        }
    }

    #[test]
    fn get_all_cols_conflicts_with_col() {
        let res = Cli::try_parse_from(["norn", "get", "a.md", "--all-cols", "--col", "type"]);
        assert!(res.is_err(), "--all-cols and --col are mutually exclusive");
    }

    #[test]
    fn get_parses_sort_and_paging() {
        let cli = Cli::try_parse_from([
            "norn",
            "get",
            "a.md",
            "--sort",
            "modified",
            "--desc",
            "--limit",
            "5",
            "--starts-at",
            "2",
        ])
        .unwrap();
        match cli.command {
            Command::Get(args) => {
                assert_eq!(args.paging.sort.as_deref(), Some("modified"));
                assert!(args.paging.desc);
                assert_eq!(args.paging.limit, Some(5));
                assert_eq!(args.paging.starts_at, 2);
            }
            _ => panic!("expected Get variant"),
        }
    }

    #[test]
    fn get_limit_conflicts_with_no_limit() {
        let res = Cli::try_parse_from(["norn", "get", "a.md", "--limit", "5", "--no-limit"]);
        assert!(
            res.is_err(),
            "--limit and --no-limit are mutually exclusive"
        );
    }

    #[test]
    fn get_parses_col_narrowing() {
        let cli = Cli::try_parse_from(["norn", "get", "a.md", "--col", "incoming_links"]).unwrap();
        match cli.command {
            Command::Get(args) => {
                assert_eq!(args.col, vec!["incoming_links".to_string()]);
            }
            _ => panic!("expected Get variant"),
        }
    }

    #[test]
    fn get_format_defaults_records() {
        let cli = Cli::try_parse_from(["norn", "get", "a.md"]).unwrap();
        match cli.command {
            Command::Get(args) => assert_eq!(args.format, GetFormat::Records),
            _ => panic!("expected Get variant"),
        }
    }
}

#[cfg(test)]
mod set_cli_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn set_requires_a_doc_argument() {
        assert!(Cli::try_parse_from(["norn", "set"]).is_err());
    }

    #[test]
    fn set_accepts_field_flag() {
        let cli = Cli::try_parse_from(["norn", "set", "Notes/foo.md", "--field", "status=active"])
            .unwrap();
        match cli.command {
            Command::Set(args) => {
                assert_eq!(args.target, "Notes/foo.md");
                assert_eq!(args.fields, vec!["status=active".to_string()]);
            }
            _ => panic!("expected Command::Set"),
        }
    }

    #[test]
    fn set_accepts_push_pop_remove_flags() {
        let cli = Cli::try_parse_from([
            "norn",
            "set",
            "doc.md",
            "--push",
            "aliases=foo",
            "--pop",
            "aliases=bar",
            "--remove",
            "old_key",
        ])
        .unwrap();
        match cli.command {
            Command::Set(args) => {
                assert_eq!(args.push, vec!["aliases=foo".to_string()]);
                assert_eq!(args.pop, vec!["aliases=bar".to_string()]);
                assert_eq!(args.remove, vec!["old_key".to_string()]);
            }
            _ => panic!("expected Command::Set"),
        }
    }

    #[test]
    fn set_accepts_field_json_and_body_from_stdin_and_force_and_yes_and_dry_run() {
        let cli = Cli::try_parse_from([
            "norn",
            "set",
            "doc.md",
            "--field-json",
            "count=42",
            "--body-from-stdin",
            "--force",
            "--yes",
            "--dry-run",
            "--format",
            "json",
        ])
        .unwrap();
        match cli.command {
            Command::Set(args) => {
                assert_eq!(args.field_json, vec!["count=42".to_string()]);
                assert!(args.body_from_stdin);
                assert!(args.force);
                assert!(args.yes);
                assert!(args.dry_run);
                assert_eq!(args.format, SetFormat::Json);
            }
            _ => panic!("expected Command::Set"),
        }
    }
}

#[cfg(test)]
mod move_cli_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn move_subcommand_parses_with_all_flags() {
        let cli = Cli::try_parse_from([
            "norn",
            "move",
            "src.md",
            "dst.md",
            "--yes",
            "--dry-run",
            "--no-link-rewrite",
            "--force",
            "--parents",
            "--recursive",
            "--format",
            "json",
        ]);
        assert!(cli.is_ok(), "parse error: {:?}", cli.err());
    }

    #[test]
    fn move_subcommand_parses_parents_short_flag() {
        let cli = Cli::try_parse_from(["norn", "move", "src.md", "dst.md", "-p"]);
        assert!(cli.is_ok(), "parse error: {:?}", cli.err());
        match cli.unwrap().command {
            Command::Move(args) => assert!(args.parents),
            _ => panic!("expected Move"),
        }
    }

    #[test]
    fn move_subcommand_parses_recursive_short_flag() {
        let cli = Cli::try_parse_from(["norn", "move", "src_dir", "dst_dir", "-r"]);
        assert!(cli.is_ok(), "parse error: {:?}", cli.err());
        match cli.unwrap().command {
            Command::Move(args) => assert!(args.recursive),
            _ => panic!("expected Move"),
        }
    }
}

#[cfg(test)]
mod delete_cli_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn delete_subcommand_parses_with_all_flags() {
        // --allow-broken-links and --rewrite-to conflict; test each combo separately.
        // This variant exercises --rewrite-to path.
        let cli = Cli::try_parse_from([
            "norn",
            "delete",
            "old.md",
            "--yes",
            "--dry-run",
            "--rewrite-to",
            "new.md",
            "--format",
            "json",
        ]);
        assert!(cli.is_ok(), "parse error: {:?}", cli.err());

        // Also verify --allow-broken-links path (without --rewrite-to).
        let cli2 = Cli::try_parse_from([
            "norn",
            "delete",
            "old.md",
            "--yes",
            "--dry-run",
            "--allow-broken-links",
            "--format",
            "json",
        ]);
        assert!(cli2.is_ok(), "parse error: {:?}", cli2.err());
    }

    #[test]
    fn delete_allow_broken_links_and_rewrite_to_are_mutually_exclusive() {
        let cli = Cli::try_parse_from([
            "norn",
            "delete",
            "old.md",
            "--allow-broken-links",
            "--rewrite-to",
            "new.md",
        ]);
        assert!(cli.is_err(), "expected mutually-exclusive error");
    }
}

#[cfg(test)]
mod new_cli_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_new_with_path_and_fields() {
        let cli = Cli::try_parse_from([
            "norn",
            "new",
            "Workspaces/foo/tasks/bar.md",
            "--field",
            "description=hello",
            "--field",
            "priority=high",
        ])
        .unwrap();
        match cli.command {
            Command::New(args) => {
                assert_eq!(
                    args.path.as_ref().map(|p| p.as_str()),
                    Some("Workspaces/foo/tasks/bar.md")
                );
                assert_eq!(args.field.len(), 2);
            }
            _ => panic!("expected Command::New"),
        }
    }

    #[test]
    fn parses_new_with_parents_short_flag() {
        let cli = Cli::try_parse_from(["norn", "new", "a/b/c.md", "-p"]).unwrap();
        match cli.command {
            Command::New(args) => assert!(args.parents),
            _ => panic!("expected Command::New"),
        }
    }

    #[test]
    fn parses_new_with_force_body_stdin_yes_dryrun() {
        let cli = Cli::try_parse_from([
            "norn",
            "new",
            "a.md",
            "--force",
            "--body-from-stdin",
            "--yes",
            "--dry-run",
        ])
        .unwrap();
        match cli.command {
            Command::New(args) => {
                assert!(args.force);
                assert!(args.body_from_stdin);
                assert!(args.yes);
                assert!(args.dry_run);
            }
            _ => panic!("expected Command::New"),
        }
    }

    #[test]
    fn parses_new_with_field_json() {
        let cli = Cli::try_parse_from(["norn", "new", "a.md", "--field-json", r#"tags=["a","b"]"#])
            .unwrap();
        match cli.command {
            Command::New(args) => {
                assert_eq!(args.field_json.len(), 1);
            }
            _ => panic!("expected Command::New"),
        }
    }
}
