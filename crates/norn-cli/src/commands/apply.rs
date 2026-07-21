//! `norn apply <plan>` — execute an already-reviewed `MigrationPlan`.
//!
//! Ported from the donor `apply::{preamble, run_direct, route}` (ADR 0018). The
//! client-side PREAMBLE runs before any wire activity: read the plan source (file
//! or stdin `-`), detect its format (`.yaml`/`.yml` → YAML, else JSON; stdin
//! defaults JSON; `--input-format` overrides), parse it into a `MigrationPlan`,
//! and validate its `schema_version`. A malformed plan or an unreadable source is
//! an operational [`Diagnostic`]; a schema mismatch is a report-shaped refusal
//! (exit 2) byte-identical to the donor's records prose. Only a parsed,
//! schema-valid plan crosses the wire — TYPED in [`ApplyParams::plan`] as the
//! `MigrationPlan` itself — so the plan bytes reviewed are the plan bytes applied
//! (ADR 0011). The owner executes it under its single-writer lock and answers with
//! the shared typed [`ApplyReport`] the display layer renders.

use std::io::Read;

use crate::cli::{ApplyArgs, ApplyFormat, GlobalArgs, InputFormat};
use crate::display::{ApplyMutationView, Diagnostic, Output};
use norn_wire::ApplyParams;
use norn_wire::{ApplyError, ApplyReport};
use norn_wire::{MigrationPlan, MIGRATION_PLAN_SCHEMA_VERSION};

/// Run an `apply` and return its report as an [`Output`], or a soft-landing
/// [`Diagnostic`] on a bad/unreadable plan or a connection/owner failure. A
/// schema mismatch and every owner-side pre-write decline arrive IN the report
/// (`outcome = refused`) the display renders at exit 2.
pub fn run(args: &ApplyArgs, global: &GlobalArgs) -> Result<Output, Diagnostic> {
    // --dry-run wins over --yes; no --yes is a forecast (the shared mode ladder).
    run_confirm(args, global, args.yes && !args.dry_run)
}

/// Same as [`run`], but with `confirm` supplied rather than derived from
/// `args` — the dispatch loop's interactive retry (NRN-389) calls this
/// directly with `confirm: true` after a TTY 'y' answer. This is a SECOND
/// routed request, not a replay of the cached forecast: the plan is re-read
/// and re-executed fresh under the owner's lock, exactly as a direct `--yes`
/// invocation would (the donor's own `apply` prompt worked the same way — it
/// prompted, then executed the SAME in-process `ApplyContext`, which the
/// routed owner boundary here re-runs instead of replaying).
pub(crate) fn run_confirm(
    args: &ApplyArgs,
    global: &GlobalArgs,
    confirm: bool,
) -> Result<Output, Diagnostic> {
    let json = matches!(args.format, ApplyFormat::Json);

    // ── Preamble: read + format-detect + parse + schema-check (client-side) ──
    let raw = read_plan_source(&args.plan_path)?;
    let fmt = determine_input_format(&args.plan_path, args.input_format);
    let plan = parse_plan(&raw, fmt, &args.plan_path)?;

    // A schema mismatch refuses BEFORE the wire (exit 2), byte-identical to the
    // donor's records prose. Carried as a refused `ApplyReport` so it renders
    // through the one apply-report path (records `error:` line / json envelope).
    if plan.schema_version != MIGRATION_PLAN_SCHEMA_VERSION {
        let report = ApplyReport::refused(
            plan.vault_root.clone(),
            !confirm,
            "apply",
            ApplyError {
                code: "unsupported-schema-version".into(),
                message: format!(
                    "unsupported plan schema_version {}; this norn build supports v{}",
                    plan.schema_version, MIGRATION_PLAN_SCHEMA_VERSION
                ),
                path: None,
            },
        );
        return Ok(Output::Apply(ApplyMutationView {
            report,
            json,
            out: args.out.clone(),
        }));
    }

    let params = ApplyParams {
        plan,
        confirm,
        parents: args.parents,
    };

    let mut session = crate::routed::open_session(global)?;
    let report = session
        .apply(params)
        .map_err(|e| crate::routed::client_error_diagnostic(&e))?;

    Ok(Output::Apply(ApplyMutationView {
        report,
        json,
        out: args.out.clone(),
    }))
}

/// Read the plan source: stdin for `-`, else the file at `plan_path`. A read
/// failure is an operational diagnostic (the donor's `{e:#}` chain, rewrapped
/// through the one error seam).
fn read_plan_source(plan_path: &str) -> Result<String, Diagnostic> {
    if plan_path == "-" {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf).map_err(|e| {
            Diagnostic::new(format!(
                "failed to read migration plan from '-': could not read from stdin: {e}"
            ))
        })?;
        Ok(buf)
    } else {
        std::fs::read_to_string(plan_path).map_err(|e| {
            Diagnostic::new(format!(
                "failed to read migration plan from '{plan_path}': could not read file \
                 '{plan_path}': {e}"
            ))
        })
    }
}

/// Determine the input format from an explicit override, else the path extension
/// (`.yaml`/`.yml` → YAML), else JSON (also the stdin default). Donor
/// `apply::determine_input_format`.
fn determine_input_format(plan_path: &str, override_fmt: Option<InputFormat>) -> InputFormat {
    if let Some(fmt) = override_fmt {
        return fmt;
    }
    if plan_path == "-" {
        return InputFormat::Json;
    }
    let lower = plan_path.to_ascii_lowercase();
    if lower.ends_with(".yaml") || lower.ends_with(".yml") {
        InputFormat::Yaml
    } else {
        InputFormat::Json
    }
}

/// Parse a `MigrationPlan` from raw text in the given format. A parse failure is
/// an operational diagnostic naming the source. Donor `apply::parse_plan`.
fn parse_plan(raw: &str, fmt: InputFormat, source: &str) -> Result<MigrationPlan, Diagnostic> {
    match fmt {
        InputFormat::Yaml => serde_yaml::from_str(raw).map_err(|e| {
            Diagnostic::new(format!(
                "failed to parse YAML migration plan from '{source}': {e}"
            ))
        }),
        InputFormat::Json => serde_json::from_str(raw).map_err(|e| {
            Diagnostic::new(format!(
                "failed to parse JSON migration plan from '{source}': {e}"
            ))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use clap::Parser;

    fn apply_args(argv: &[&str]) -> ApplyArgs {
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Apply(a) => a,
            other => panic!("expected apply, got {other:?}"),
        }
    }

    fn confirm(a: &ApplyArgs) -> bool {
        a.yes && !a.dry_run
    }

    #[test]
    fn confirm_ladder_dry_run_wins_over_yes() {
        assert!(
            !confirm(&apply_args(&[
                "norn",
                "apply",
                "p.json",
                "--yes",
                "--dry-run"
            ])),
            "dry-run wins over yes"
        );
        assert!(
            confirm(&apply_args(&["norn", "apply", "p.json", "--yes"])),
            "yes alone applies"
        );
        assert!(
            !confirm(&apply_args(&["norn", "apply", "p.json"])),
            "no flag forecasts"
        );
    }

    #[test]
    fn format_detection_extension_and_override() {
        assert!(matches!(
            determine_input_format("plan.yaml", None),
            InputFormat::Yaml
        ));
        assert!(matches!(
            determine_input_format("plan.yml", None),
            InputFormat::Yaml
        ));
        assert!(matches!(
            determine_input_format("plan.json", None),
            InputFormat::Json
        ));
        assert!(matches!(
            determine_input_format("plan", None),
            InputFormat::Json
        ));
        assert!(matches!(
            determine_input_format("-", None),
            InputFormat::Json
        ));
        assert!(matches!(
            determine_input_format("plan.json", Some(InputFormat::Yaml)),
            InputFormat::Yaml
        ));
    }

    #[test]
    fn malformed_json_is_a_diagnostic() {
        let err = parse_plan("{not valid", InputFormat::Json, "p.json").unwrap_err();
        assert!(err
            .message()
            .starts_with("failed to parse JSON migration plan from 'p.json'"));
    }

    /// The pre-wire schema-version refusal reports a TRUTHFUL `dry_run`: a
    /// forecast unless `--yes` is actually given (and `--dry-run` wins over
    /// `--yes`). The branch returns before any session, so this drives the real
    /// arg → confirm-ladder → refused-report path with no owner.
    #[test]
    fn schema_mismatch_refusal_dry_run_tracks_confirm_ladder() {
        use crate::display::Output;
        use norn_wire::ApplyOutcome;

        let dir = std::env::temp_dir().join(format!("norn-apply-schema-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let plan_path = dir.join("bad.plan.json");
        std::fs::write(
            &plan_path,
            r#"{ "schema_version": 99, "vault_root": ".", "operations": [] }"#,
        )
        .unwrap();
        let p = plan_path.to_str().unwrap();

        let refusal_dry_run = |argv: &[&str]| -> bool {
            let cli = Cli::try_parse_from(argv).unwrap();
            let args = match cli.command {
                Command::Apply(a) => a,
                other => panic!("expected apply, got {other:?}"),
            };
            match run(&args, &cli.global).unwrap() {
                Output::Apply(view) => {
                    assert_eq!(view.report.outcome, ApplyOutcome::Refused);
                    assert_eq!(
                        view.report.operations[0].error.as_ref().unwrap().code,
                        "unsupported-schema-version"
                    );
                    view.report.dry_run
                }
                _ => panic!("expected Output::Apply"),
            }
        };

        // No --yes: a forecast — dry_run must be true (the bug reported false here).
        assert!(
            refusal_dry_run(&["norn", "apply", p]),
            "no --yes is a forecast: refusal dry_run must be true"
        );
        // --yes: an apply — dry_run must be false.
        assert!(
            !refusal_dry_run(&["norn", "apply", p, "--yes"]),
            "--yes is an apply: refusal dry_run must be false"
        );
        // --yes --dry-run: dry-run wins — dry_run must be true.
        assert!(
            refusal_dry_run(&["norn", "apply", p, "--yes", "--dry-run"]),
            "dry-run wins over yes: refusal dry_run must be true"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
