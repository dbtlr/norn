//! `norn find` — the exemplar read command over the filter + sort/paging +
//! output surface.
//!
//! The command-module pattern, proven here and in `get`: the module owns (a)
//! its clap `Args`, (b) a `to_params` mapping into the wire vocabulary, and (c)
//! a `run` entry that presents the outcome. This phase's `run` parses to Params
//! then presents the uniform not-yet-ported outcome; the port swaps the
//! presented outcome for `execute(env, params)`'s Report (ADR 0016).
//!
//! Grammar + help text are donor-exact (NRN-329) so the custom help renderer
//! matches the parity oracle byte-for-byte on `find --help`. Doc-comment help
//! (clap strips the trailing period) is deliberate — the periods are
//! load-bearing.

use std::io::Write;

use clap::Args;
use norn_wire::{FilterParams, SortPaginateParams};

use crate::commands::args::{FilterArgs, SortPaginateArgs};
use crate::display::Presenter;

/// The command name, used in the not-yet-ported diagnostic.
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
    /// Mutually exclusive with `--col`.
    #[arg(long = "all-cols", conflicts_with = "col", help_heading = "Output")]
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

impl FindArgs {
    /// Parse the flags into the shared read-verb wire vocabulary.
    pub fn to_params(&self) -> (FilterParams, SortPaginateParams) {
        (self.filter.to_params(), self.paging.to_params())
    }
}

/// Present the command's outcome and return the process exit code.
pub fn run<O: Write, E: Write>(args: &FindArgs, presenter: &mut Presenter<O, E>) -> i32 {
    // The adapter's job: argv → Params. `execute(env, params)` and Report
    // presentation land when the read verbs port.
    let (_filter, _paging) = args.to_params();
    presenter.not_yet_ported(NAME)
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
    fn all_cols_conflicts_with_col() {
        let res = Cli::try_parse_from(["norn", "find", "--all", "--all-cols", "--col", "title"]);
        assert!(res.is_err(), "--all-cols and --col are mutually exclusive");
    }

    #[test]
    fn col_splits_on_comma() {
        let args = find_args(&["norn", "find", "--all", "--col", "title,status"]);
        assert_eq!(args.col, vec!["title".to_string(), "status".to_string()]);
    }
}
