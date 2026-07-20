//! `norn delete` — remove a document, optionally redirecting its incoming links.
//!
//! Maps the args into the wire [`DeleteParams`], resolves apply-vs-forecast from
//! the shared mode ladder, and hands them to the owner. The owner resolves the
//! target, runs the backlink-policy preflight, cascades any `--rewrite-to`
//! redirect under its single-writer lock, and answers with the shared
//! `ApplyReport` (as a JSON value) the display layer renders.

use crate::cli::{DeleteArgs, GlobalArgs};
use crate::display::{DeleteMutationView, Diagnostic, Output};
use norn_core::apply::report::ApplyReport;
use norn_wire::DeleteParams;

/// Run a `delete` mutation and return its report as an [`Output`], or a
/// soft-landing [`Diagnostic`] on a connection/owner failure. A clean pre-write
/// decline (target not found, backlinks present, …) arrives IN the report
/// (`outcome = refused`) the display renders at exit 2.
pub fn run(args: &DeleteArgs, global: &GlobalArgs) -> Result<Output, Diagnostic> {
    let params = DeleteParams {
        target: args.doc.clone(),
        rewrite_to: args.rewrite_to.clone(),
        allow_broken_links: args.allow_broken_links,
        confirm: args.yes && !args.dry_run,
    };

    let mut session = crate::routed::open_session(global)?;
    let value = session
        .delete(params)
        .map_err(|e| crate::routed::client_error_diagnostic(&e))?;
    let report: ApplyReport = serde_json::from_value(value)
        .map_err(|e| Diagnostic::new(format!("undecodable delete report: {e}")))?;

    Ok(Output::Delete(DeleteMutationView {
        report,
        doc: args.doc.clone(),
        json: matches!(args.format, crate::cli::DeleteFormat::Json),
    }))
}

#[cfg(test)]
mod tests {
    use crate::cli::{Cli, Command, DeleteArgs};
    use clap::Parser;

    fn delete_args(argv: &[&str]) -> DeleteArgs {
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Delete(a) => a,
            other => panic!("expected delete, got {other:?}"),
        }
    }

    #[test]
    fn rewrite_to_carried() {
        let a = delete_args(&["norn", "delete", "old.md", "--rewrite-to", "new.md"]);
        assert_eq!(a.rewrite_to.as_deref(), Some("new.md"));
    }
}
