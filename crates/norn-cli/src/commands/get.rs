//! `norn get` — the second exemplar read command, over one or more targets
//! plus the shared sort/paging surface. Same command-module pattern as `find`.
//!
//! Grammar + help text are donor-exact (NRN-329).

use std::io::Write;

use clap::Args;
use norn_wire::SortPaginateParams;

use crate::commands::args::SortPaginateArgs;
use crate::display::Presenter;

/// The command name, used in the not-yet-ported diagnostic.
const NAME: &str = "get";

#[derive(Args, Debug)]
pub struct GetArgs {
    /// One or more doc targets. Each accepts path, stem, or wikilink-shaped
    /// input (with or without [[]]). Anchor / block-ref / pipe-alias
    /// suffixes are stripped before resolution.
    #[arg(required = true, num_args = 1.., value_name = "DOC")]
    pub targets: Vec<String>,

    // ── Sort / limit / paging (shared with `find`) ─────────────────────
    #[command(flatten)]
    pub paging: SortPaginateArgs,

    // ── Output ───────────────────────────────────────────────────────────
    /// Emit the full structured dump: every frontmatter field plus every
    /// cache-served facet (`.headings`, the three link sets, `.body`).
    /// Mutually exclusive with `--col`.
    #[arg(long = "all-cols", conflicts_with = "col", help_heading = "Output")]
    pub all_cols: bool,

    /// Comma-separated columns to include. Bare names select frontmatter
    /// fields (e.g. `status,title`), exactly like `norn find`. Structural
    /// facets are dot-prefixed: `.path`, `.stem`, `.frontmatter` (the whole
    /// block), `.headings`, `.outgoing_links`, `.unresolved_links`,
    /// `.incoming_links`, `.body`, `.document_hash` (the content hash
    /// `edit --expected-hash` wants; opt-in only — never in `--all-cols`).
    /// Without --col, frontmatter +
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

    /// Output format. Default records; markdown returns one exact source file.
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

impl GetArgs {
    /// Parse the sort/paging flags into the shared wire vocabulary. The targets
    /// carry through as the verb's own request field once `get` ports.
    pub fn to_params(&self) -> SortPaginateParams {
        self.paging.to_params()
    }
}

/// Present the command's outcome and return the process exit code.
pub fn run<O: Write, E: Write>(args: &GetArgs, presenter: &mut Presenter<O, E>) -> i32 {
    let _paging = args.to_params();
    presenter.not_yet_ported(NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use clap::Parser;

    fn get_args(argv: &[&str]) -> GetArgs {
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Get(a) => a,
            other => panic!("expected get, got {other:?}"),
        }
    }

    #[test]
    fn targets_are_collected_and_paging_defaults() {
        let args = get_args(&["norn", "get", "alpha", "notes/beta.md"]);
        assert_eq!(args.targets, vec!["alpha", "notes/beta.md"]);
        assert_eq!(args.to_params(), SortPaginateParams::default());
    }

    #[test]
    fn get_requires_at_least_one_target() {
        assert!(Cli::try_parse_from(["norn", "get"]).is_err());
    }

    #[test]
    fn all_cols_conflicts_with_col() {
        let res = Cli::try_parse_from(["norn", "get", "a.md", "--all-cols", "--col", "type"]);
        assert!(res.is_err(), "--all-cols and --col are mutually exclusive");
    }

    #[test]
    fn format_defaults_records() {
        let args = get_args(&["norn", "get", "a.md"]);
        assert_eq!(args.format, GetFormat::Records);
    }
}
