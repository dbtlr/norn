//! `norn move` — relocate a document (or folder) and cascade-rewrite backlinks.
//!
//! Maps the args into the wire [`MoveParams`], resolves apply-vs-forecast from
//! the shared mode ladder (`--dry-run` forecasts, `--yes` applies, otherwise a
//! safe implicit forecast), and hands them to the owner. The owner resolves the
//! source, plans the move, drives the cascade under its single-writer lock, and
//! answers with the shared `ApplyReport` (as a JSON value); the display layer
//! renders it (records summary / pretty json) and derives the exit code.

use crate::cli::{GlobalArgs, MoveArgs};
use crate::display::{Diagnostic, MoveMutationView, Output};
use norn_core::apply::report::ApplyReport;
use norn_wire::MoveParams;

/// Run a `move` mutation and return its report as an [`Output`], or a
/// soft-landing [`Diagnostic`] on a connection/owner failure. A clean pre-write
/// decline arrives IN the report (`outcome = refused`) the display renders at
/// exit 2.
pub fn run(args: &MoveArgs, global: &GlobalArgs) -> Result<Output, Diagnostic> {
    let params = MoveParams {
        from: args.src.clone(),
        to: args.dst.clone(),
        recursive: args.recursive,
        parents: args.parents,
        force: args.force,
        no_link_rewrite: args.no_link_rewrite,
        // --dry-run wins over --yes; no --yes is a forecast.
        confirm: args.yes && !args.dry_run,
    };

    let mut session = crate::routed::open_session(global)?;
    let value = session
        .move_document(params)
        .map_err(|e| crate::routed::client_error_diagnostic(&e))?;
    let report: ApplyReport = serde_json::from_value(value)
        .map_err(|e| Diagnostic::new(format!("undecodable move report: {e}")))?;

    Ok(Output::Move(MoveMutationView {
        report,
        src: args.src.clone(),
        dst: args.dst.clone(),
        json: matches!(args.format, crate::cli::MoveFormat::Json),
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
