//! `norn set` — mutate a document's frontmatter fields.
//!
//! The command merges the `--field`/positional `KEY=VALUE` tokens (ADR 0010
//! sugar) into the wire [`SetParams`], resolves the apply-vs-forecast decision
//! from the mode ladder, reads `--body-from-stdin` when asked, and hands the
//! params to the owner. The owner builds and — when `confirm` is set — applies
//! the plan under its single-writer lock, and answers with a [`SetReport`] the
//! display layer renders (records change-lines / json).
//!
//! The apply-vs-forecast ladder (client-side, ADR 0011 amendment): `--dry-run`
//! forecasts; `--yes` applies; everything else forecasts (a safe implicit
//! dry-run). Agents drive `--yes` / `--dry-run` / `--format json` and are
//! fully served by that ladder alone; a human at a TTY additionally gets the
//! donor's preview → prompt → apply conversation (NRN-389), wired through
//! [`run_confirm`] and `display::emit_mutation`.

use std::io::Read;

use crate::cli::{GlobalArgs, SetArgs, SetFormat};
use crate::display::{Diagnostic, Format, FormatChoice, FormatSpec, Output, SetMutationView};
use norn_wire::SetParams;

impl From<SetFormat> for Format {
    fn from(f: SetFormat) -> Self {
        match f {
            SetFormat::Records => Format::Records,
            SetFormat::Json => Format::Json,
        }
    }
}

/// Run a `set` mutation and return its report as an [`Output`], or a
/// soft-landing [`Diagnostic`] on a connection/owner failure. A clean pre-write
/// decline is NOT a `Diagnostic` — it arrives as a report with `outcome =
/// refused` the display layer renders at exit 2.
pub fn run(args: &SetArgs, global: &GlobalArgs) -> Result<Output, Diagnostic> {
    run_confirm(args, global, args.yes && !args.dry_run)
}

/// Same as [`run`], but with `confirm` supplied rather than derived from
/// `args` — the dispatch loop's interactive retry (NRN-389) calls this
/// directly with `confirm: true` after a TTY 'y' answer. This is a SECOND
/// routed request, not a replay of the cached forecast: the owner re-plans
/// and applies fresh under its lock, exactly as a direct `--yes` invocation
/// would.
pub(crate) fn run_confirm(
    args: &SetArgs,
    global: &GlobalArgs,
    confirm: bool,
) -> Result<Output, Diagnostic> {
    let body = if args.body_from_stdin {
        Some(read_stdin()?)
    } else {
        None
    };

    // Merge the trailing positional KEY=VALUE tokens into --field (ADR 0010): the
    // owner's synth parses and validates them (an unseparated token is an
    // `assignment-malformed` refusal, exit 2).
    let mut fields = args.fields.clone();
    fields.extend(args.field_pos.iter().cloned());

    let params = SetParams {
        target: args.target.clone(),
        fields,
        field_json: args.field_json.clone(),
        push: args.push.clone(),
        pop: args.pop.clone(),
        remove: args.remove.clone(),
        body,
        force: args.force,
        confirm,
    };

    let mut session = crate::routed::open_session(global)?;
    let report = session
        .set(params)
        .map_err(|e| crate::routed::client_error_diagnostic(&e))?;

    Ok(Output::Set(SetMutationView {
        report,
        format: FormatChoice {
            explicit: Some(args.format.into()),
            // Mutations do not switch format on isatty — the mode ladder decides
            // apply-vs-forecast, and `--format` (default records) decides shape.
            spec: FormatSpec {
                tty: Format::Records,
                piped: Format::Records,
            },
        },
    }))
}

/// Read the whole of stdin as the replacement body (`--body-from-stdin`).
fn read_stdin() -> Result<String, Diagnostic> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| Diagnostic::new(format!("could not read body from stdin: {e}")))?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use clap::Parser;

    fn set_args(argv: &[&str]) -> SetArgs {
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Set(a) => a,
            other => panic!("expected set, got {other:?}"),
        }
    }

    #[test]
    fn positionals_merge_into_fields() {
        let args = set_args(&["norn", "set", "a.md", "status=done", "kind=note"]);
        let mut fields = args.fields.clone();
        fields.extend(args.field_pos.iter().cloned());
        assert_eq!(
            fields,
            vec!["status=done".to_string(), "kind=note".to_string()]
        );
    }

    /// The `confirm = yes && !dry_run` ladder: dry-run wins over yes, yes alone
    /// applies, no flag forecasts.
    fn confirm(a: &SetArgs) -> bool {
        a.yes && !a.dry_run
    }

    #[test]
    fn confirm_ladder_dry_run_wins_over_yes() {
        assert!(
            !confirm(&set_args(&[
                "norn",
                "set",
                "a.md",
                "x=1",
                "--yes",
                "--dry-run"
            ])),
            "dry-run must win over yes"
        );
        assert!(
            confirm(&set_args(&["norn", "set", "a.md", "x=1", "--yes"])),
            "--yes alone applies"
        );
        assert!(
            !confirm(&set_args(&["norn", "set", "a.md", "x=1"])),
            "no flag is a forecast"
        );
    }
}
