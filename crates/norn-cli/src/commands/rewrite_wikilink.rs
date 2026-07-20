//! `norn rewrite-wikilink OLD NEW` — rewrite every `[[old]]` reference to
//! `[[new]]` across the vault (body + frontmatter).
//!
//! Maps the args into the wire [`RewriteWikilinkParams`], resolves
//! apply-vs-forecast from the shared mode ladder, and hands them to the owner.
//! The owner resolves `old`, expands the per-document rewrites, applies under its
//! single-writer lock, and answers with the shared `ApplyReport` (as a JSON
//! value) the display layer renders. `--out` writes the JSON report to a file.

use crate::cli::{GlobalArgs, RewriteWikilinkArgs};
use crate::display::{Diagnostic, Output, RewriteWikilinkView};
use norn_core::apply::report::ApplyReport;
use norn_wire::RewriteWikilinkParams;

/// Run a `rewrite-wikilink` mutation and return its report as an [`Output`], or a
/// soft-landing [`Diagnostic`] on a connection/owner failure. An unresolvable
/// `OLD` arrives IN the report (`outcome = refused`) the display renders at
/// exit 2.
pub fn run(args: &RewriteWikilinkArgs, global: &GlobalArgs) -> Result<Output, Diagnostic> {
    let params = RewriteWikilinkParams {
        old: args.old.clone(),
        new: args.new.clone(),
        confirm: args.yes && !args.dry_run,
    };

    let mut session = crate::routed::open_session(global)?;
    let value = session
        .rewrite_wikilink(params)
        .map_err(|e| crate::routed::client_error_diagnostic(&e))?;
    let report: ApplyReport = serde_json::from_value(value)
        .map_err(|e| Diagnostic::new(format!("undecodable rewrite-wikilink report: {e}")))?;

    Ok(Output::RewriteWikilink(RewriteWikilinkView {
        report,
        old: args.old.clone(),
        new: args.new.clone(),
        json: matches!(args.format, crate::cli::RewriteWikilinkFormat::Json),
        out: args.out.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use crate::cli::{Cli, Command, RewriteWikilinkArgs};
    use clap::Parser;

    fn rw_args(argv: &[&str]) -> RewriteWikilinkArgs {
        match Cli::try_parse_from(argv).unwrap().command {
            Command::RewriteWikilink(a) => a,
            other => panic!("expected rewrite-wikilink, got {other:?}"),
        }
    }

    #[test]
    fn old_new_carried() {
        let a = rw_args(&["norn", "rewrite-wikilink", "foo", "bar"]);
        assert_eq!(a.old, "foo");
        assert_eq!(a.new, "bar");
    }
}
