//! `norn apply <plan>` — execute an already-reviewed `MigrationPlan`.
//!
//! Ported from the donor `apply::{preamble, run_direct, route}` (ADR 0018). The
//! client-side PREAMBLE runs before any wire activity: read the plan source (file
//! or stdin `-`), detect its format (`.yaml`/`.yml` → YAML, else JSON; stdin
//! defaults JSON; `--input-format` overrides), and parse it into a
//! `MigrationPlan`. A malformed plan or an unreadable source is an operational
//! [`Diagnostic`]. The parsed plan crosses the wire — TYPED in
//! [`ApplyParams::plan`] as the `MigrationPlan` itself — so the plan bytes
//! reviewed are the plan bytes applied (ADR 0011). The owner executes it under its
//! single-writer lock and answers with the shared typed `ApplyReport` the display
//! layer renders. The `schema_version` gate lives ONCE in the shared engine
//! (`apply_migration_plan`, audit-F3), not here: a mismatch returns a coded
//! `unsupported-schema-version` refused report (exit 2) from the owner, so the
//! routed/MCP surface is guarded on the same path as the CLI.

use std::io::Read;

use crate::cli::{ApplyArgs, GlobalArgs, InputFormat};
use crate::display::{ApplyMutationView, Diagnostic, Format, FormatChoice, FormatSpec, Output};
use norn_wire::ApplyParams;
use norn_wire::MigrationPlan;

/// Run an `apply` and return its report as an [`Output`], or a soft-landing
/// [`Diagnostic`] on a bad/unreadable plan or a connection/owner failure. A
/// schema mismatch and every owner-side pre-write decline arrive IN the report
/// (`outcome = refused`) the display renders at exit 2.
pub fn run(args: &ApplyArgs, global: &GlobalArgs) -> Result<Output, Diagnostic> {
    // --dry-run wins over --yes; no --yes is a forecast (the shared mode ladder).
    run_confirm(args, global, args.mode.confirm())
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
    // ── Preamble: read + format-detect + parse (client-side) ──
    // The `schema_version` gate lives ONCE in the shared engine
    // (`apply_migration_plan`, audit-F3), so the routed/MCP surface is guarded too;
    // a mismatch returns a coded `unsupported-schema-version` refused report (exit
    // 2) from the owner, rendered through the one apply-report path.
    let raw = read_plan_source(&args.plan_path)?;
    let fmt = determine_input_format(&args.plan_path, args.input_format);
    let plan = parse_plan(&raw, fmt, &args.plan_path)?;

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
        format: FormatChoice {
            explicit: Some(args.mode.format.into()),
            spec: FormatSpec {
                tty: Format::Records,
                piped: Format::Records,
            },
        },
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

    // NOTE (NRN-406, audit-F3): the former
    // `schema_mismatch_refusal_dry_run_tracks_confirm_ladder` CLI test is removed.
    // The `schema_version` gate moved out of the client-side preamble into the
    // shared engine (`apply_migration_plan`), so the refusal no longer originates
    // "before any session" — it now returns from the owner as a coded refused
    // report. The refused report's `dry_run`-tracks-`confirm` behavior it pinned is
    // covered engine-side by
    // `crate::mutate::apply::tests::unsupported_schema_version_refuses_with_zero_ops_examined`
    // (which loops `confirm` over both values), and the `confirm`-ladder derivation
    // itself is covered by the `confirm` unit tests above.
}
