//! `norn validate` — run the standards engine over the vault graph and surface
//! findings. Read-only (no repair, no mutation).
//!
//! The command builds the triage filters into the wire [`ValidateParams`], the
//! owner runs the engine + filter against its warm graph, and `run` returns the
//! [`ValidateReport`](norn_wire::ValidateReport) as an [`Output`]; the display
//! layer projects `--summary` / `--format` (records / jsonl / json / paths). The
//! records human view composes from the severity-tally + by-code primitives.

use crate::cli::{GlobalArgs, ValidateArgs, ValidateFormat};
use crate::display::{Diagnostic, Format, FormatSpec, Output, ValidateView};
use norn_wire::ValidateParams;

impl From<ValidateFormat> for Format {
    fn from(f: ValidateFormat) -> Self {
        match f {
            ValidateFormat::Records => Format::Records,
            ValidateFormat::Jsonl => Format::Jsonl,
            ValidateFormat::Json => Format::Json,
            ValidateFormat::Paths => Format::Paths,
        }
    }
}

/// Run the validate engine over the target vault and return its report as an
/// [`Output`], or a soft-landing [`Diagnostic`] on failure.
pub fn run(args: &ValidateArgs, global: &GlobalArgs) -> Result<Output, Diagnostic> {
    let mut session = crate::routed::open_session(global)?;

    let params = ValidateParams {
        codes: args.triage.code.clone(),
        severities: args.triage.severity.clone(),
        fields: args.triage.field.clone(),
        rules: args.triage.rule.clone(),
        paths: args.triage.path.clone(),
        targets: args.triage.target.clone(),
        reasons: args.triage.reason.clone(),
        verbose: global.verbose,
        summary: args.summary,
    };

    let report = session
        .validate(params)
        .map_err(|e| crate::routed::client_error_diagnostic(&e))?;

    Ok(Output::Validate(ValidateView {
        report,
        summary: args.summary,
        explicit: args.format.map(Format::from),
        // Donor default: records on a tty, jsonl when piped.
        spec: FormatSpec {
            tty: Format::Records,
            piped: Format::Jsonl,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use clap::Parser;

    fn validate_args(argv: &[&str]) -> ValidateArgs {
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Validate(a) => a,
            other => panic!("expected validate, got {other:?}"),
        }
    }

    #[test]
    fn triage_filters_collect() {
        let args = validate_args(&[
            "norn",
            "validate",
            "--code",
            "link-target-missing",
            "--severity",
            "warning",
        ]);
        assert_eq!(args.triage.code, vec!["link-target-missing".to_string()]);
        assert_eq!(args.triage.severity, vec!["warning".to_string()]);
        assert!(!args.summary);
        assert!(args.format.is_none());
    }

    #[test]
    fn summary_and_format_parse() {
        let args = validate_args(&["norn", "validate", "--summary", "--format", "json"]);
        assert!(args.summary);
        assert_eq!(Format::from(args.format.unwrap()), Format::Json);
    }
}
