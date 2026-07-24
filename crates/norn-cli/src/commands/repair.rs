//! `norn repair` — surface deterministic-repair findings and, with `--plan`,
//! emit a `MigrationPlan` from them. Read-only: repair builds the plan (the
//! owner runs the validate engine, filters findings, and generates the plan);
//! `norn apply` executes it.
//!
//! The command maps the triage/confidence/skip-reason knobs into the wire
//! [`RepairParams`], the owner answers with the [`RepairReport`] (the plan as its
//! JSON string plus the bare-summary tally + exit signal), and `run` returns it
//! as an [`Output`]; the display layer projects bare-summary vs `--plan`
//! (records / json / paths) and honors `--out`.

use crate::cli::{ConfidenceArg, GlobalArgs, RepairArgs};
use crate::display::{Diagnostic, Output, RepairView};
use norn_wire::RepairParams;

/// Run a `repair` request and return its report as an [`Output`], or a
/// soft-landing [`Diagnostic`] on a connection/owner failure.
pub fn run(args: &RepairArgs, global: &GlobalArgs) -> Result<Output, Diagnostic> {
    let params = RepairParams {
        codes: args.triage.code.clone(),
        severities: args.triage.severity.clone(),
        fields: args.triage.field.clone(),
        rules: args.triage.rule.clone(),
        paths: args.triage.path.clone(),
        targets: args.triage.target.clone(),
        reasons: args.triage.reason.clone(),
        skip_reasons: args.skip_reason.clone(),
        confidence_high: matches!(args.confidence, Some(ConfidenceArg::High)),
        verbose: global.verbose,
    };

    let mut session = crate::routed::open_session(global)?;
    let report = session
        .repair(params)
        .map_err(|e| crate::routed::client_error_diagnostic(&e))?;

    Ok(Output::Repair(RepairView {
        report,
        plan: args.plan,
        format: args.format,
        out: args.out.clone(),
        filter_flags: active_filter_flags(args),
    }))
}

/// The active triage/confidence/skip-reason flags, reconstructed as an argv
/// fragment for the `--format records` apply-guidance lines. Glob-shaped values
/// are single-quoted so the
/// printed command copy-pastes safely.
fn active_filter_flags(args: &RepairArgs) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(ConfidenceArg::High) = args.confidence {
        out.push("--confidence".into());
        out.push("high".into());
    }
    let repeatable = [
        ("--skip-reason", &args.skip_reason),
        ("--code", &args.triage.code),
        ("--severity", &args.triage.severity),
        ("--field", &args.triage.field),
        ("--rule", &args.triage.rule),
        ("--path", &args.triage.path),
        ("--target", &args.triage.target),
        ("--reason", &args.triage.reason),
    ];
    for (flag, values) in repeatable {
        for value in values {
            out.push(flag.into());
            out.push(quote_if_glob(value));
        }
    }
    out
}

fn quote_if_glob(s: &str) -> String {
    if s.contains('*') || s.contains('?') || s.contains('[') {
        format!("'{s}'")
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use clap::Parser;

    fn repair_args(argv: &[&str]) -> RepairArgs {
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Repair(a) => a,
            other => panic!("expected repair, got {other:?}"),
        }
    }

    #[test]
    fn triage_and_confidence_collect() {
        let args = repair_args(&[
            "norn",
            "repair",
            "--plan",
            "--code",
            "link-*",
            "--confidence",
            "high",
        ]);
        assert!(args.plan);
        assert_eq!(args.triage.code, vec!["link-*".to_string()]);
        assert!(matches!(args.confidence, Some(ConfidenceArg::High)));
    }

    #[test]
    fn active_filter_flags_quote_globs() {
        let args = repair_args(&[
            "norn", "repair", "--plan", "--code", "link-*", "--rule", "r1",
        ]);
        let flags = active_filter_flags(&args);
        assert_eq!(
            flags,
            vec!["--code", "'link-*'", "--rule", "r1"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
    }

    /// Bare `repair --format paths` (no `--plan`) has no defined projection —
    /// the bare output is a findings summary, not a document enumeration — so
    /// clap refuses it at parse time (`requires_if`) rather than silently
    /// rendering the records summary.
    #[test]
    fn bare_format_paths_without_plan_is_a_usage_error() {
        let err = Cli::try_parse_from(["norn", "repair", "--format", "paths"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn plan_format_paths_parses() {
        let args = repair_args(&["norn", "repair", "--plan", "--format", "paths"]);
        assert!(args.plan);
        assert!(matches!(
            args.format,
            Some(crate::cli::RepairPlanFormat::Paths)
        ));
    }
}
