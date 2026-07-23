//! `norn new` — create a document from a rule template, an explicit path, or the
//! inbox fallback.
//!
//! The command maps the creation-mode inputs into the wire [`NewParams`], reads
//! `--body-from-stdin` when asked, and resolves apply-vs-forecast from the same
//! mode ladder as `set` (`--dry-run` forecasts, `--yes` applies, otherwise a safe
//! implicit forecast). The owner resolves defaults / `{{seq}}` and — when
//! confirmed — writes the file under its single-writer lock, answering with a
//! [`NewReport`](norn_wire::NewReport) the display layer renders.

use std::io::Read;

use crate::cli::{GlobalArgs, NewArgs};
use crate::display::{Diagnostic, Format, FormatChoice, FormatSpec, NewMutationView, Output};
use norn_wire::NewParams;

/// Run a `new` creation and return its report as an [`Output`], or a
/// soft-landing [`Diagnostic`] on a connection/owner failure. A clean pre-write
/// decline (unknown rule, destination exists, …) arrives as a report with
/// `outcome = refused` the display layer renders at exit 2.
pub fn run(args: &NewArgs, global: &GlobalArgs) -> Result<Output, Diagnostic> {
    run_confirm(args, global, args.mode.confirm())
}

/// Same as [`run`], but with `confirm` supplied rather than derived from
/// `args` — the dispatch loop's interactive retry (NRN-389) calls this
/// directly with `confirm: true` after a TTY 'y' answer. This is a SECOND
/// routed request, not a replay of the cached forecast: the owner re-resolves
/// defaults/`{{seq}}` and writes fresh under its lock, exactly as a direct
/// `--yes` invocation would.
pub(crate) fn run_confirm(
    args: &NewArgs,
    global: &GlobalArgs,
    confirm: bool,
) -> Result<Output, Diagnostic> {
    let body = if args.body_from_stdin {
        Some(read_stdin()?)
    } else {
        None
    };

    let params = NewParams {
        path: args.path.as_ref().map(|p| p.to_string_lossy().into_owned()),
        as_rule: args.as_rule.clone(),
        title: args.title.clone(),
        vars: args.var.clone(),
        fields: args.field.clone(),
        field_json: args.field_json.clone(),
        body,
        parents: args.parents,
        force: args.force,
        confirm,
    };

    let mut session = crate::routed::open_session(global)?;
    let report = session
        .new_document(params)
        .map_err(|e| crate::routed::client_error_diagnostic(&e))?;

    Ok(Output::New(NewMutationView {
        report,
        format: FormatChoice {
            explicit: Some(args.mode.format.into()),
            spec: FormatSpec {
                tty: Format::Records,
                piped: Format::Records,
            },
        },
    }))
}

/// Read the whole of stdin as the new document's body (`--body-from-stdin`).
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

    fn new_args(argv: &[&str]) -> NewArgs {
        match Cli::try_parse_from(argv).unwrap().command {
            Command::New(a) => a,
            other => panic!("expected new, got {other:?}"),
        }
    }

    #[test]
    fn by_rule_mode_carries_the_rule_name() {
        let args = new_args(&["norn", "new", "--as", "task", "--title", "Ship it"]);
        assert_eq!(args.as_rule.as_deref(), Some("task"));
        assert_eq!(args.title.as_deref(), Some("Ship it"));
        assert!(args.path.is_none());
    }

    #[test]
    fn explicit_path_mode_carries_the_path() {
        let args = new_args(&["norn", "new", "notes/a.md"]);
        assert_eq!(
            args.path.as_ref().map(|p| p.to_string_lossy().into_owned()),
            Some("notes/a.md".to_string())
        );
        assert!(args.as_rule.is_none());
    }
}
