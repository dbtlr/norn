//! `norn find` — the filtered/sorted/paged document query.
//!
//! The command module owns its clap `Args`, the `to_params` mapping into the
//! wire vocabulary, and `run`: help-gate → summon → `FindParams` → return the
//! `FindReport` as an [`Output`] for the display layer to render (NRN-370).
//! The rendering (paths / records / json / jsonl) lives once in `display::emit`.
//!
//! Deep facets (`.headings`, `.outgoing_links`, `.unresolved_links`,
//! `.incoming_links`) and `--all-cols` load the matches' full connection sets
//! (NRN-347). The CLI sets [`FindParams::with_connections`] only when one of
//! those facets is requested, so an unrequested deep facet is never rendered as
//! a misleading empty array — the empty-vs-loaded distinction lives here, where
//! the request is known.

use clap::Args;
use norn_wire::{FindParams, SortPaginateParams};

use crate::cli::GlobalArgs;
use crate::commands::args::{FilterArgs, SortPaginateArgs};
use crate::display::{Diagnostic, FindView, Format, FormatChoice, FormatSpec, Output};
use crate::output::projection::split_cols;

/// The deep connection facets that require a per-match connection load. When any
/// is requested (or `--all-cols`), the CLI sets `with_connections` so the owner
/// loads them; otherwise a plain `find` never pays that cost.
const DEEP_FACETS: &[&str] = &[
    "headings",
    "outgoing_links",
    "unresolved_links",
    "incoming_links",
];

const NAME: &str = "find";

#[derive(Args, Debug)]
pub struct FindArgs {
    // ── Filter predicates ──────────────────────────────────────────────
    #[command(flatten)]
    pub filter: FilterArgs,

    /// Return every document — escape hatch when no predicate is specified.
    /// Without --all and without any predicate, `norn find` prints its help
    /// page (a full-vault dump is almost always a mistake; require opt-in).
    #[arg(long, help_heading = "Filter options")]
    pub all: bool,

    // ── Sort / limit / paging (shared with `get`) ───────────────────────
    #[command(flatten)]
    pub paging: SortPaginateArgs,

    // ── Output ───────────────────────────────────────────────────────────
    /// Output format. Default auto-detects: TTY → records, piped → paths.
    #[arg(long, value_enum, help_heading = "Output")]
    pub format: Option<FindFormat>,

    /// Emit the full structured dump for each match: whole frontmatter plus
    /// every cache-served facet (`.headings`, the three link sets, `.body`).
    /// Competes with `--col` over the projection; the last of the two given wins.
    #[arg(long = "all-cols", overrides_with = "col", help_heading = "Output")]
    pub all_cols: bool,

    /// Comma-separated columns to include. Bare names select frontmatter
    /// fields (e.g. `status,title`), exactly like `norn get`. Structural
    /// facets are dot-prefixed: `.path`, `.stem`, `.frontmatter` (the whole
    /// block), `.headings`, `.outgoing_links`, `.unresolved_links`,
    /// `.incoming_links`, `.body`, `.document_hash` (the content hash
    /// `edit --expected-hash` wants; opt-in only — never in `--all-cols`).
    /// Default (no --col): frontmatter
    /// only. Ignored with a warning on paths format.
    #[arg(
        long,
        value_name = "COL1,COL2,...",
        value_delimiter = ',',
        overrides_with = "all_cols",
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

impl From<FindFormat> for Format {
    fn from(f: FindFormat) -> Self {
        match f {
            FindFormat::Paths => Format::Paths,
            FindFormat::Records => Format::Records,
            FindFormat::Json => Format::Json,
            FindFormat::Jsonl => Format::Jsonl,
        }
    }
}

impl FindArgs {
    /// Parse the flags into the shared read-verb wire vocabulary.
    pub fn to_params(&self) -> (norn_wire::FilterParams, SortPaginateParams) {
        (self.filter.to_params(), self.paging.to_params())
    }

    /// Whether the request needs each match's deep connection facets loaded —
    /// true for `--all-cols` or any deep `--col` facet.
    fn wants_connections(&self) -> bool {
        if self.all_cols {
            return true;
        }
        let (facets, _fields) = split_cols(&self.col);
        facets.iter().any(|f| DEEP_FACETS.contains(&f.as_str()))
    }

    /// Whether any filter predicate is present (an empty `--text` is not one).
    /// Compared against the empty default so a new predicate flag can never be
    /// silently missed.
    fn has_predicate(&self) -> bool {
        let mut probe = self.filter.clone();
        if probe.text.as_deref() == Some("") {
            probe.text = None;
        }
        probe != FilterArgs::default()
    }
}

/// Resolve the query and return its report as an [`Output`] for the layer to
/// render, or a soft-landing [`Diagnostic`] on failure.
pub fn run(args: &FindArgs, global: &GlobalArgs) -> Result<Output, Diagnostic> {
    // Help gate: bare `find` (no predicate, no --all) returns its help page for
    // the layer to write on stderr with exit 2 — a full-vault dump is almost
    // always a mistake.
    if !args.all && !args.has_predicate() {
        return Ok(Output::Usage(crate::help::render_command_long(
            NAME,
            global.color,
        )));
    }

    let mut session = crate::routed::open_session(global)?;

    let (filter, paging) = args.to_params();
    let params = FindParams {
        filter,
        paging,
        with_connections: args.wants_connections(),
        // The desugared dynamic-field keys ride to the owner's field-universe
        // gate (NRN-367); the owner rejects a genuinely-unknown field with a
        // did-you-mean instead of returning a silent empty set.
        dynamic_keys: global.dynamic_fields.clone(),
    };
    let report = session
        .find(params)
        .map_err(|e| crate::routed::client_error_diagnostic(&e))?;

    Ok(Output::Find(FindView {
        report,
        cols: args.col.clone(),
        all_cols: args.all_cols,
        sort_field: args.paging.sort.clone(),
        no_pager: args.no_pager,
        format: FormatChoice {
            explicit: args.format.map(Format::from),
            // The one tty-sensitive default pair: records on a terminal, paths piped.
            spec: FormatSpec {
                tty: Format::Records,
                piped: Format::Paths,
            },
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use clap::Parser;

    fn find_args(argv: &[&str]) -> FindArgs {
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Find(a) => a,
            other => panic!("expected find, got {other:?}"),
        }
    }

    #[test]
    fn find_format_parses() {
        let args = find_args(&["norn", "find", "--all", "--format", "json"]);
        assert_eq!(args.format, Some(FindFormat::Json));
    }

    #[test]
    fn repeated_format_is_last_wins_grammar_wide() {
        // NRN-365: `--format` has no per-arg self-override; the root's
        // `args_override_self` makes the repeat last-wins.
        let args = find_args(&[
            "norn", "find", "--all", "--format", "json", "--format", "paths",
        ]);
        assert_eq!(args.format, Some(FindFormat::Paths));
    }

    #[test]
    fn repeatable_append_flags_still_accumulate_under_last_wins() {
        // `args_override_self` touches only self-conflicting `Set` flags; an
        // `Append` predicate like `--eq` still accumulates both values.
        let args = find_args(&["norn", "find", "--eq", "type:note", "--eq", "status:active"]);
        assert_eq!(
            args.filter.eq,
            vec!["type:note".to_string(), "status:active".to_string()]
        );
    }

    #[test]
    fn all_cols_and_col_are_last_wins() {
        // NRN-331: --all-cols and --col compete over the projection; the last
        // one on the command line wins (no hard conflict).
        let col_last = find_args(&["norn", "find", "--all", "--all-cols", "--col", "title"]);
        assert!(!col_last.all_cols, "--col is last, so --all-cols is reset");
        assert_eq!(col_last.col, vec!["title".to_string()]);

        let all_cols_last = find_args(&["norn", "find", "--all", "--col", "title", "--all-cols"]);
        assert!(all_cols_last.all_cols, "--all-cols is last, so it wins");
        assert!(
            all_cols_last.col.is_empty(),
            "the overridden --col is reset"
        );
    }

    #[test]
    fn col_splits_on_comma() {
        let args = find_args(&["norn", "find", "--all", "--col", "title,status"]);
        assert_eq!(args.col, vec!["title".to_string(), "status".to_string()]);
    }

    #[test]
    fn has_predicate_false_for_bare_find() {
        let args = find_args(&["norn", "find", "--all"]);
        assert!(!args.has_predicate());
    }

    #[test]
    fn has_predicate_true_for_eq() {
        let args = find_args(&["norn", "find", "--eq", "type:note"]);
        assert!(args.has_predicate());
    }

    #[test]
    fn empty_text_is_not_a_predicate() {
        let args = find_args(&["norn", "find", "--text", ""]);
        assert!(!args.has_predicate());
    }

    // ── NRN-347 deep facets: --all-cols / a deep --col loads connections ──

    #[test]
    fn wants_connections_only_for_deep_cols_or_all_cols() {
        assert!(find_args(&["norn", "find", "--all", "--all-cols"]).wants_connections());
        assert!(find_args(&["norn", "find", "--all", "--col", ".headings"]).wants_connections());
        assert!(
            find_args(&["norn", "find", "--all", "--col", ".incoming_links"]).wants_connections()
        );
        // Flat facets and bare fields never trigger the connection load.
        assert!(!find_args(&["norn", "find", "--all", "--col", ".stem"]).wants_connections());
        assert!(!find_args(&["norn", "find", "--all", "--col", "title"]).wants_connections());
        assert!(!find_args(&["norn", "find", "--all"]).wants_connections());
    }
}
