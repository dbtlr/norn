//! `norn get` — one or more documents in detail, over the shared sort/paging
//! surface. Same command-module pattern as `find`.
//!
//! Grammar + help text are donor-exact (NRN-329). The owner resolves the targets
//! and returns each document's full facet set (frontmatter, headings, the three
//! link sets, hash/stem, and body when asked); `run` returns the `GetReport` as an
//! [`Output`], and the display layer projects `--col` / `--all-cols` / `--section`
//! / `--format` byte-faithfully to the donor (NRN-370). `--format markdown` prints
//! the exact source bytes the owner read from disk (ADR 0014).

use clap::Args;
use norn_wire::{GetParams, SortPaginateParams};

use crate::cli::GlobalArgs;
use crate::commands::args::SortPaginateArgs;
use crate::display::{Diagnostic, Format, FormatChoice, FormatSpec, GetView, Output};
use crate::output::projection::split_cols;

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
    /// Competes with `--col` over the projection; the last of the two given wins.
    #[arg(long = "all-cols", overrides_with = "col", help_heading = "Output")]
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
        overrides_with = "all_cols",
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

impl From<GetFormat> for Format {
    fn from(f: GetFormat) -> Self {
        match f {
            GetFormat::Records => Format::Records,
            GetFormat::Paths => Format::Paths,
            GetFormat::Json => Format::Json,
            GetFormat::Jsonl => Format::Jsonl,
            GetFormat::Markdown => Format::Markdown,
        }
    }
}

impl GetArgs {
    /// Parse the sort/paging flags into the shared wire vocabulary.
    pub fn to_params(&self) -> SortPaginateParams {
        self.paging.to_params()
    }

    /// Whether the whole-body field should be displayed — `--all-cols` or a
    /// `--col .body` request. Drives [`GetParams::with_body`].
    fn wants_body_display(&self) -> bool {
        if self.all_cols {
            return true;
        }
        let (facets, _fields) = split_cols(&self.col);
        facets.iter().any(|f| f == "body")
    }

    /// Whether this format consumes `--section` (records / json / jsonl). `paths`
    /// / `markdown` document it as ignored, so the CLI does not send it — that
    /// keeps the owner from resolving a heading whose miss would push an
    /// exit-flipping `error:` note into a format that never renders sections.
    fn format_consumes_sections(&self) -> bool {
        matches!(
            self.format,
            GetFormat::Records | GetFormat::Json | GetFormat::Jsonl
        )
    }
}

/// Resolve the targets and return the report as an [`Output`] for the layer to
/// render, or a soft-landing [`Diagnostic`] on failure.
pub fn run(args: &GetArgs, global: &GlobalArgs) -> Result<Output, Diagnostic> {
    let mut session = crate::routed::open_session(global)?;

    let params = GetParams {
        targets: args.targets.clone(),
        paging: args.to_params(),
        sections: if args.format_consumes_sections() {
            args.section.clone()
        } else {
            Vec::new()
        },
        with_body: args.wants_body_display(),
        markdown: matches!(args.format, GetFormat::Markdown),
    };

    let report = session
        .get(params)
        .map_err(|e| crate::routed::client_error_diagnostic(&e))?;

    Ok(Output::Get(GetView {
        report,
        cols: args.col.clone(),
        sections: args.section.clone(),
        sort_field: args.paging.sort.clone(),
        format: FormatChoice {
            explicit: Some(args.format.into()),
            spec: FormatSpec {
                tty: Format::Records,
                piped: Format::Records,
            },
        },
    }))
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
    fn all_cols_and_col_are_last_wins() {
        // NRN-331: last of --all-cols / --col over the projection wins.
        let col_last = get_args(&["norn", "get", "a.md", "--all-cols", "--col", "type"]);
        assert!(!col_last.all_cols);
        assert_eq!(col_last.col, vec!["type".to_string()]);

        let all_cols_last = get_args(&["norn", "get", "a.md", "--col", "type", "--all-cols"]);
        assert!(all_cols_last.all_cols);
        assert!(all_cols_last.col.is_empty());
    }

    #[test]
    fn format_defaults_records() {
        let args = get_args(&["norn", "get", "a.md"]);
        assert_eq!(args.format, GetFormat::Records);
    }

    #[test]
    fn wants_body_display_only_for_all_cols_or_dot_body() {
        assert!(get_args(&["norn", "get", "a.md", "--all-cols"]).wants_body_display());
        assert!(get_args(&["norn", "get", "a.md", "--col", ".body"]).wants_body_display());
        assert!(!get_args(&["norn", "get", "a.md"]).wants_body_display());
        assert!(!get_args(&["norn", "get", "a.md", "--col", "title"]).wants_body_display());
    }
}
