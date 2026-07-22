//! `norn move` — relocate a document (or folder) and cascade-rewrite backlinks.
//!
//! Maps the args into the wire [`MoveParams`], resolves apply-vs-forecast from
//! the shared mode ladder (`--dry-run` forecasts, `--yes` applies, otherwise a
//! safe implicit forecast), and hands them to the owner. The owner resolves the
//! source, plans the move, drives the cascade under its single-writer lock, and
//! answers with the shared typed `ApplyReport`; the display layer renders it
//! (records summary / pretty json) and derives the exit code.

use crate::cli::{GlobalArgs, MoveArgs, MoveFormat};
use crate::display::{Diagnostic, Format, FormatChoice, FormatSpec, MoveMutationView, Output};
use norn_wire::MoveParams;

impl From<MoveFormat> for Format {
    fn from(f: MoveFormat) -> Self {
        match f {
            MoveFormat::Records => Format::Records,
            MoveFormat::Json => Format::Json,
        }
    }
}

/// Run a `move` mutation and return its report as an [`Output`], or a
/// soft-landing [`Diagnostic`] on a connection/owner failure. A clean pre-write
/// decline arrives IN the report (`outcome = refused`) the display renders at
/// exit 2.
pub fn run(args: &MoveArgs, global: &GlobalArgs) -> Result<Output, Diagnostic> {
    run_confirm(args, global, args.yes && !args.dry_run)
}

/// Same as [`run`], but with `confirm` supplied rather than derived from
/// `args` — the dispatch loop's interactive retry (NRN-389) calls this
/// directly with `confirm: true` after a TTY 'y' answer. This is a SECOND
/// routed request, not a replay of the cached forecast: the owner re-resolves
/// the source and re-plans the move + cascade fresh under its lock, exactly
/// as a direct `--yes` invocation would.
pub(crate) fn run_confirm(
    args: &MoveArgs,
    global: &GlobalArgs,
    confirm: bool,
) -> Result<Output, Diagnostic> {
    let params = MoveParams {
        from: args.src.clone(),
        to: args.dst.clone(),
        recursive: args.recursive,
        parents: args.parents,
        force: args.force,
        no_link_rewrite: args.no_link_rewrite,
        confirm,
    };

    let mut session = crate::routed::open_session(global)?;
    let report = session
        .move_document(params)
        .map_err(|e| crate::routed::client_error_diagnostic(&e))?;

    Ok(Output::Move(MoveMutationView {
        report,
        src: args.src.clone(),
        dst: args.dst.clone(),
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
    use crate::cli::{Cli, Command, MoveArgs};
    use clap::Parser;

    fn move_args(argv: &[&str]) -> MoveArgs {
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Move(a) => a,
            other => panic!("expected move, got {other:?}"),
        }
    }

    fn confirm(a: &MoveArgs) -> bool {
        a.yes && !a.dry_run
    }

    #[test]
    fn confirm_ladder_dry_run_wins_over_yes() {
        assert!(
            !confirm(&move_args(&[
                "norn",
                "move",
                "a.md",
                "b.md",
                "--yes",
                "--dry-run"
            ])),
            "dry-run wins over yes"
        );
        assert!(
            confirm(&move_args(&["norn", "move", "a.md", "b.md", "--yes"])),
            "yes alone applies"
        );
        assert!(
            !confirm(&move_args(&["norn", "move", "a.md", "b.md"])),
            "no flag forecasts"
        );
    }
}
