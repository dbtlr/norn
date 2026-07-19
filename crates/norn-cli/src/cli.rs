//! The clap command surface for the `norn` binary: the `Cli` parser, the
//! global flags, and the `Command` enum with one variant per verb.
//!
//! Declarations only — no command logic. `crate::dispatch` matches on the
//! `Command` this produces and hands each variant to its command module (or the
//! uniform not-yet-ported stub). The full v0.48 grammar is ported here (NRN-329)
//! so the interface contract is frozen against the parity oracle: names, shorts,
//! aliases, value names, help headings, arg groups, conflicts, and defaults are
//! donor-exact. Help is NOT emitted by clap — the root and every subcommand set
//! `disable_help_flag` + `disable_help_subcommand`, and `crate::help` renders the
//! custom help by walking this derive tree.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::commands::{find::FindArgs, get::GetArgs, vault::VaultCmd};

#[derive(Debug, Parser)]
#[command(name = "norn")]
#[command(about = "Deterministic Markdown vault graph tools")]
#[command(version)]
#[command(disable_help_flag = true)]
#[command(disable_help_subcommand = true)]
// NRN-365: grammar-wide last-wins. `args_override_self` is a GLOBAL clap setting
// (`AppSettings::AllArgsOverrideSelf`) — declared once on the root, it propagates
// to every subcommand, so EVERY scalar flag repeat (`ArgAction::Set`) resolves to
// the last occurrence instead of erroring `ArgumentConflict`. One lever replaces
// per-arg `overrides_with_self` across the whole surface. It touches only a
// flag's conflict-with-ITSELF: repeatable `Append` flags (`--eq`, `--field`,
// `--col`) still accumulate, and genuine CROSS-flag rules (`--all-cols`/`--col`
// via `overrides_with`, delete's `--allow-broken-links`/`--rewrite-to` via
// `conflicts_with`) are unaffected and still enforced.
#[command(args_override_self = true)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,

    #[command(subcommand)]
    pub command: Command,
}

// Flags accepted before or after the subcommand (`global = true`). Parsed but
// not yet wired to behavior — vault resolution lands with the summoner
// (`norn-client`), which will consume these to pick the target vault.
//
// A plain comment, NOT a doc comment: clap adopts a flattened struct's doc
// comment as the parent command's `long_about`, which would leak into the
// top-level `--help` and diverge from the oracle (whose root has none).
//
// Order is load-bearing: the custom help renderer lists globals in declaration
// order, so `cwd, verbose, no-cache-refresh, color, vault` reproduces this
// build's GLOBAL OPTIONS block. The two help bools are excluded from that block
// by the extractor.
//
// Divergence from the pinned oracle (0.48), decision-gated in the parity ledger
// (NRN-345): the global `--config` is DELETED — under the registered-vault model
// (ADR 0017) the per-vault config is resolver-derived
// (`[vaults.<name>].config -> <root>/.norn/config.yaml`), not a free-floating
// global path — and the new-world `--vault NAME` global is UNHIDDEN, since
// name-addressed routing (`norn-client`) now consumes it. Both changes reshape
// every command's GLOBAL OPTIONS block, covered by ledger entries PD-101 /
// PD-102.
#[derive(Debug, Args)]
pub struct GlobalArgs {
    // NRN-335: the concise `help` fits the ≤70-char GLOBAL OPTIONS column shown
    // in `-h`; the full default-resolution chain moves to `long_help`, which
    // `--help` renders UNCLAMPED. Splitting the two stops `-h` from showing an
    // ellipsis-truncated line while keeping the whole story available in `--help`.
    #[arg(
        short = 'C',
        long,
        global = true,
        help_heading = "Global options",
        help = "Run as if norn started in this directory",
        long_help = "Run as if norn started in this directory (default: $NORN_ROOT, else the current directory)"
    )]
    pub cwd: Option<PathBuf>,

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

    /// Target the registered vault with this name (ADR 0017). Now exposed —
    /// the summoner (`norn-client`) resolves it through the registry to pick the
    /// vault an invocation routes to. A decided-better divergence from the pinned
    /// oracle, ledger entries PD-101 / PD-102.
    #[arg(
        long,
        global = true,
        value_name = "NAME",
        help_heading = "Global options",
        help = "Target the registered vault with this name (see `norn vault register`)"
    )]
    pub vault: Option<String>,

    #[arg(
        short = 'h',
        global = true,
        help_heading = "Global options",
        help = "Print short help. Use --help for full help",
        action = clap::ArgAction::SetTrue
    )]
    pub help_short: bool,

    #[arg(
        long = "help",
        global = true,
        help_heading = "Global options",
        help = "Print full help. Use -h for a short summary",
        action = clap::ArgAction::SetTrue
    )]
    pub help_long: bool,

    /// The dynamically-desugared field keys the forgiving-input normalization
    /// (ADR 0010) expanded from `--field value` predicates, captured BEFORE clap
    /// parses and injected here after parse (`run`). Not a real flag — `skip`
    /// keeps it out of the grammar and help; it rides the parsed command into
    /// the query verbs, which forward it to the owner-side field-universe gate
    /// (NRN-367). Canonical `--eq`/`--in` keys never appear here.
    #[arg(skip)]
    pub dynamic_fields: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorWhen {
    Always,
    Auto,
    Never,
}

// clap's `Subcommand` derive requires each variant's payload to impl `Args`,
// which `Box<T>` does not — so the lint's boxing fix is unavailable here.
#[allow(clippy::large_enum_variant)]
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
        disable_help_flag = true,
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
        about = "Update one document — schema-aware frontmatter mutation + wholesale body replacement"
    )]
    Set(SetArgs),
    #[command(
        disable_help_flag = true,
        about = "Edit one document's body — atomic content-anchored partial edits"
    )]
    Edit(EditArgs),
    #[command(
        disable_help_flag = true,
        about = "Create a new document — schema-aware frontmatter pre-fill from path rules"
    )]
    New(NewArgs),
    #[command(disable_help_flag = true, about = "Scaffold .norn/config.yaml")]
    Init(InitArgs),
    #[command(
        disable_help_flag = true,
        about = "Move/rename a document with cascading backlink rewrites"
    )]
    Move(MoveArgs),
    #[command(
        name = "delete",
        disable_help_flag = true,
        about = "Delete a document, optionally redirecting incoming links to an alternate target"
    )]
    Delete(DeleteArgs),
    #[command(
        disable_help_flag = true,
        about = "Apply a MigrationPlan — execute move, delete, rewrite, and frontmatter ops from a plan file"
    )]
    Apply(ApplyArgs),
    #[command(
        disable_help_flag = true,
        about = "Surface deterministic-repair findings; --plan emits a MigrationPlan"
    )]
    Repair(RepairArgs),
    #[command(
        name = "rewrite-wikilink",
        disable_help_flag = true,
        about = "Rewrite all occurrences of a wikilink target across the vault (body + frontmatter)"
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
        about = "Manage the SQLite-backed vault graph cache"
    )]
    Cache(CacheCommand),
    #[command(
        disable_help_flag = true,
        about = "Manage the per-vault `.norn/config.yaml`"
    )]
    Config(ConfigCommand),
    #[command(
        disable_help_flag = true,
        about = "Update norn to the latest GitHub release"
    )]
    SelfUpdate(SelfUpdateArgs),
    #[command(
        disable_help_flag = true,
        about = "Run norn as a Model Context Protocol (MCP) stdio server over the vault at --cwd"
    )]
    Mcp(McpArgs),
    #[command(
        disable_help_flag = true,
        about = "Run the warm host daemon: serves MCP for any vault on this host over a Unix socket"
    )]
    Serve(ServeArgs),
    #[command(
        disable_help_flag = true,
        about = "Supervise the warm `norn serve` daemon under launchd (macOS): install/uninstall/start/stop/restart/status"
    )]
    Service(ServiceCommand),
    #[command(
        disable_help_flag = true,
        about = "Read the vault mutation audit trail (the append-only event stream): recent mutations with status / target / trace, filterable"
    )]
    Audit(AuditArgs),
    /// The intentionally-new registered-vault namespace (ADR 0017). No oracle
    /// counterpart — the pinned oracle (0.48) predates it — so it is the
    /// canonical decided-better divergence in the top-level help (PD-101):
    /// it appears in the COMMANDS list the oracle lacks. Placed after every
    /// ported oracle command so it is the sole extra row.
    #[command(
        subcommand,
        disable_help_flag = true,
        about = "Manage the vault registry — register a vault to unlock durable artifacts (cache, event stream, logs)"
    )]
    Vault(VaultCmd),
    #[command(
        hide = true,
        disable_help_flag = true,
        about = "Emit roff-format man page to stdout"
    )]
    Manpage,
}

#[derive(Debug, Args)]
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
    pub filters: crate::commands::args::FilterArgs,

    /// Output format. Default text (records-block).
    #[arg(long, value_enum, default_value_t = CountFormat::Text, help_heading = "Output")]
    pub format: CountFormat,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountFormat {
    Text,
    Json,
}

#[derive(Debug, Args)]
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
    pub filters: crate::commands::args::FilterArgs,

    /// Output format. Default text.
    #[arg(long, value_enum, help_heading = "Output")]
    pub format: Option<DescribeFormat>,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescribeFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Args)]
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
pub enum ValidateFormat {
    /// Human-legible records (TTY default).
    Records,
    /// One JSON object per finding, streaming.
    Jsonl,
    /// Single JSON object wrapper with a `findings` array.
    Json,
    /// One path per affected document, sorted and deduped.
    Paths,
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
    pub out: Option<PathBuf>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairPlanFormat {
    Report,
    Json,
    Paths,
}

fn parse_repair_plan_format(s: &str) -> Result<RepairPlanFormat, String> {
    match s {
        "report" => Ok(RepairPlanFormat::Report),
        "json" => Ok(RepairPlanFormat::Json),
        "paths" => Ok(RepairPlanFormat::Paths),
        "jsonl" => Err("jsonl was removed — use --format json".into()),
        "table" => Err("table was removed — use --format report".into()),
        _ => Err("possible values: report, json, paths".into()),
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[clap(rename_all = "snake_case")]
pub enum ConfidenceArg {
    High,
}

#[derive(Debug, Args)]
pub struct SetArgs {
    /// The doc to mutate. Path, stem, or wikilink-shaped (with or without [[]]).
    #[arg(value_name = "DOC")]
    pub target: String,

    /// Set a frontmatter field. Repeatable; multiple instances of the same key
    /// accumulate into an array. KEY=VALUE.
    #[arg(long = "field", value_name = "KEY=VALUE")]
    pub fields: Vec<String>,

    /// Trailing `KEY=VALUE` positionals — sugar for `--field KEY=VALUE` (ADR
    /// 0010). Identical semantics: same schema coercion, same
    /// repeat-accumulates-to-array. Every positional AFTER DOC must contain a
    /// `:` or `=` separator or it is a hard error; the first positional is
    /// always DOC (a doc literally named `a=b.md` is still addressed as DOC).
    #[arg(value_name = "KEY=VALUE")]
    pub field_pos: Vec<String>,

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

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetFormat {
    Records,
    Json,
}

#[derive(Debug, Args)]
pub struct EditArgs {
    /// The doc to edit. Path, stem, or wikilink-shaped (with or without [[]]).
    #[arg(value_name = "DOC")]
    pub target: String,

    /// The edits as a JSON array of ops (e.g.
    /// `[{"op":"str_replace","old":"a","new":"b"}]`). Mutually exclusive with
    /// stdin; if omitted, the array is read from stdin.
    #[arg(long = "edits-json", value_name = "JSON")]
    pub edits_json: Option<String>,

    /// Read the ops JSON array from a file (hidden alias for stdin redirection).
    #[arg(long = "ops-file", value_name = "PATH", hide = true)]
    pub ops_file: Option<String>,

    /// Sugar: `str_replace` — the OLD anchor. Payload: `--new` (alias `--content`).
    #[arg(long = "str-replace", value_name = "OLD")]
    pub str_replace: Option<String>,

    /// Sugar: `replace_section` — the HEADING anchor. Payload: `--content`.
    #[arg(long = "replace-section", value_name = "HEADING")]
    pub replace_section: Option<String>,

    /// Sugar: `append_to_section` — the HEADING anchor. Payload: `--content`.
    #[arg(long = "append-to-section", value_name = "HEADING")]
    pub append_to_section: Option<String>,

    /// Sugar: `delete_section` — the HEADING anchor. No payload.
    #[arg(long = "delete-section", value_name = "HEADING")]
    pub delete_section: Option<String>,

    /// Sugar: `insert_before_heading` — the HEADING anchor. Payload: `--content`.
    #[arg(long = "insert-before-heading", value_name = "HEADING")]
    pub insert_before_heading: Option<String>,

    /// Sugar: `insert_after_heading` — the HEADING anchor. Payload: `--content`.
    #[arg(long = "insert-after-heading", value_name = "HEADING")]
    pub insert_after_heading: Option<String>,

    /// Payload for `--str-replace` (the replacement text; JSON field `new`).
    #[arg(long = "new", value_name = "NEW")]
    pub new: Option<String>,

    /// Payload for the section ops (JSON field `content`); also an alias for
    /// `--new` on `--str-replace`.
    #[arg(long = "content", value_name = "BODY")]
    pub content: Option<String>,

    /// Replace all matches for `--str-replace` (JSON field `replace_all`).
    #[arg(long = "replace-all")]
    pub replace_all: bool,

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

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum EditFormat {
    Records,
    Json,
}

#[derive(Debug, Args)]
pub struct NewArgs {
    /// Vault-relative path of the new document (must end in .md).
    /// Optional: omit when using --as or the inbox fallback.
    pub path: Option<PathBuf>,

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

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewFormat {
    Records,
    Json,
}

#[derive(Debug, Args)]
pub struct InitArgs {
    #[arg(long, help = "Overwrite an existing .norn/config.yaml")]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct MoveArgs {
    /// Source: vault-relative path or unique stem.
    #[arg(value_name = "FROM")]
    pub src: String,

    /// Destination: vault-relative path.
    #[arg(value_name = "TO")]
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

    /// When FROM and TO are directories, recursively move all .md files
    /// preserving structure (one cascade pass for all backlinks).
    #[arg(long, short = 'r')]
    pub recursive: bool,

    /// Stdout format. `records` is the default TTY summary; `json` emits the ApplyReport.
    #[arg(long, value_enum, default_value_t = MoveFormat::Records)]
    pub format: MoveFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum MoveFormat {
    Records,
    Json,
}

#[derive(Debug, Args)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DeleteFormat {
    Records,
    Json,
}

#[derive(Debug, Args)]
pub struct ApplyArgs {
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
    #[arg(long, value_enum, default_value_t = ApplyFormat::Records)]
    pub format: ApplyFormat,

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ApplyFormat {
    Records,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum InputFormat {
    Json,
    Yaml,
}

#[derive(Debug, Args)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RewriteWikilinkFormat {
    Records,
    Json,
}

#[derive(Debug, Args)]
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

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditFormat {
    Records,
    Json,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditStatus {
    Applied,
    Skipped,
    Failed,
}

#[derive(Debug, Args)]
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

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfUpdateFormat {
    Text,
    Json,
}

#[derive(Debug, Args)]
pub struct McpArgs {}

/// Arguments for `norn serve`. Phase 1 has no flags.
#[derive(Debug, Args)]
pub struct ServeArgs {}

#[derive(Debug, Parser)]
#[command(disable_help_flag = true)]
pub struct ServiceCommand {
    #[command(subcommand)]
    pub command: ServiceSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum ServiceSubcommand {
    #[command(
        disable_help_flag = true,
        about = "Render the launchd plist and load the serve daemon (idempotent)"
    )]
    Install(ServiceActionArgs),
    #[command(
        disable_help_flag = true,
        about = "Unload and remove the plist; config and logs are kept"
    )]
    Uninstall(ServiceActionArgs),
    #[command(
        disable_help_flag = true,
        about = "Load an installed-but-stopped serve daemon"
    )]
    Start(ServiceActionArgs),
    #[command(
        disable_help_flag = true,
        about = "Unload the serve daemon (honest stop; KeepAlive would resurrect a killed pid)"
    )]
    Stop(ServiceActionArgs),
    #[command(
        disable_help_flag = true,
        about = "Kill and rerun the loaded serve daemon (kickstart -k)"
    )]
    Restart(ServiceActionArgs),
    #[command(
        disable_help_flag = true,
        about = "Show host service health; optionally include one vault's serving/writer state"
    )]
    Status(ServiceStatusArgs),
}

#[derive(Debug, Args)]
pub struct ServiceActionArgs {
    /// Output format. Default text; `json` emits a machine-readable object.
    #[arg(long, value_enum, default_value_t = ServiceFormat::Text, help_heading = "Output")]
    pub format: ServiceFormat,
}

#[derive(Debug, Args)]
pub struct ServiceStatusArgs {
    /// Canonicalize and report this vault's serving and writer-progress state.
    #[arg(long, value_name = "PATH")]
    pub vault: Option<PathBuf>,
    /// Output format. Default text; `json` emits a machine-readable object.
    #[arg(long, value_enum, default_value_t = ServiceFormat::Text, help_heading = "Output")]
    pub format: ServiceFormat,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceFormat {
    Text,
    Json,
}

#[derive(Debug, Parser)]
#[command(disable_help_flag = true)]
pub struct CacheCommand {
    #[command(subcommand)]
    pub command: CacheSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum CacheSubcommand {
    #[command(disable_help_flag = true, about = "Update the cache incrementally")]
    Index(CacheIndexArgs),
    #[command(disable_help_flag = true, about = "Rebuild the cache from scratch")]
    Rebuild,
    #[command(
        disable_help_flag = true,
        about = "Delete the entire cache entry, without opening it first"
    )]
    Clear,
    #[command(
        disable_help_flag = true,
        about = "Show cache path, size, document and link counts, and schema version"
    )]
    Status(CacheStatusArgs),
    #[command(
        disable_help_flag = true,
        about = "Evict dead, aged, and over-cap cache entries across all vaults"
    )]
    Prune(CachePruneArgs),
    #[command(
        hide = true,
        disable_help_flag = true,
        about = "Internal: run the detached cross-vault cache GC sweep (NRN-287)"
    )]
    Sweep,
}

#[derive(Debug, Args)]
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

#[derive(Debug, Args)]
pub struct CacheStatusArgs {
    #[arg(long, value_enum, default_value_t = CacheOutputFormat::Text, help = "Stdout format")]
    pub format: CacheOutputFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CacheOutputFormat {
    Text,
    Json,
}

#[derive(Debug, Args)]
pub struct CachePruneArgs {
    #[arg(long, help = "Report what would be evicted without deleting anything")]
    pub dry_run: bool,
    #[arg(
        long,
        value_name = "RETENTION",
        help = "Age-eviction window override (e.g. 90d, 12w, 24h)"
    )]
    pub retention: Option<String>,
    #[arg(long, value_enum, default_value_t = CacheOutputFormat::Text, help = "Stdout format")]
    pub format: CacheOutputFormat,
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
        about = "Emit a shell completion script to stdout"
    )]
    Init(CompletionsInitArgs),
    #[command(
        disable_help_flag = true,
        about = "Install completions into the user's shell config"
    )]
    Install(CompletionsInstallArgs),
}

#[derive(Debug, Args)]
pub struct CompletionsInitArgs {
    #[arg(value_enum, help = "Target shell")]
    pub shell: SupportedShell,
}

#[derive(Debug, Args)]
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

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFormat {
    Records,
    Json,
    Jsonl,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use clap::Parser;

    #[test]
    fn global_cwd_accepted_before_subcommand() {
        let cli = Cli::try_parse_from(["norn", "-C", "/x", "find", "--all"]).unwrap();
        assert_eq!(cli.global.cwd.as_deref(), Some(std::path::Path::new("/x")));
    }

    #[test]
    fn global_cwd_accepted_after_subcommand() {
        let cli = Cli::try_parse_from(["norn", "find", "--all", "-C", "/x"]).unwrap();
        assert_eq!(cli.global.cwd.as_deref(), Some(std::path::Path::new("/x")));
    }

    #[test]
    fn vault_name_flag_parses() {
        let cli = Cli::try_parse_from(["norn", "--vault", "atlas", "find", "--all"]).unwrap();
        assert_eq!(cli.global.vault.as_deref(), Some("atlas"));
    }

    #[test]
    fn vault_flag_parses_after_subcommand() {
        let cli = Cli::try_parse_from(["norn", "get", "alpha", "--vault", "atlas"]).unwrap();
        assert_eq!(cli.global.vault.as_deref(), Some("atlas"));
    }

    #[test]
    fn removed_global_config_is_a_parse_error() {
        // The global `--config` was deleted (ADR 0017 resolver-derived config);
        // it must no longer parse as a global flag.
        assert!(Cli::try_parse_from(["norn", "get", "alpha", "--config", "/c.yaml"]).is_err());
    }

    #[test]
    fn color_defaults_auto() {
        let cli = Cli::try_parse_from(["norn", "find", "--all"]).unwrap();
        assert_eq!(cli.global.color, ColorWhen::Auto);
    }

    #[test]
    fn global_verbose_and_no_cache_refresh_parse() {
        let cli = Cli::try_parse_from(["norn", "--verbose", "--no-cache-refresh", "find", "--all"])
            .unwrap();
        assert!(cli.global.verbose);
        assert!(cli.global.no_cache_refresh);
    }

    #[test]
    fn unknown_command_is_a_parse_error() {
        assert!(Cli::try_parse_from(["norn", "nope"]).is_err());
    }

    #[test]
    fn count_parses_with_by_flag() {
        let cli = Cli::try_parse_from(["norn", "count", "--by", "status"]).unwrap();
        match cli.command {
            Command::Count(args) => assert_eq!(args.by, vec!["status".to_string()]),
            _ => panic!("expected Count variant"),
        }
    }

    #[test]
    fn validate_parses_summary_and_triage() {
        let cli =
            Cli::try_parse_from(["norn", "validate", "--summary", "--code", "link-*"]).unwrap();
        match cli.command {
            Command::Validate(args) => {
                assert!(args.summary);
                assert_eq!(args.triage.code, vec!["link-*".to_string()]);
            }
            _ => panic!("expected Validate variant"),
        }
    }

    #[test]
    fn move_parses_short_flags() {
        let cli = Cli::try_parse_from(["norn", "move", "src.md", "dst.md", "-p", "-r"]).unwrap();
        match cli.command {
            Command::Move(args) => {
                assert!(args.parents);
                assert!(args.recursive);
            }
            _ => panic!("expected Move variant"),
        }
    }

    #[test]
    fn delete_allow_broken_and_rewrite_to_conflict() {
        let res = Cli::try_parse_from([
            "norn",
            "delete",
            "old.md",
            "--allow-broken-links",
            "--rewrite-to",
            "new.md",
        ]);
        assert!(res.is_err(), "expected mutually-exclusive error");
    }

    #[test]
    fn cache_index_subcommand_parses() {
        let cli = Cli::try_parse_from(["norn", "cache", "index", "--force-hash"]).unwrap();
        assert!(matches!(cli.command, Command::Cache(_)));
    }

    #[test]
    fn manpage_is_hidden_but_parses() {
        let cli = Cli::try_parse_from(["norn", "manpage"]).unwrap();
        assert!(matches!(cli.command, Command::Manpage));
    }

    #[test]
    fn derive_tree_is_valid() {
        // Catches derive-level ambiguities (duplicate flags, bad global setup,
        // help-flag conflicts) at test time rather than first real invocation.
        Cli::command().debug_assert();
    }
}
