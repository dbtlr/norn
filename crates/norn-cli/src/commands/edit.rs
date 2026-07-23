//! `norn edit` — atomic, content-anchored partial edits to one document's body.
//!
//! The command resolves the ops CLI-side — single-op sugar (`--str-replace` &
//! co., ADR 0010) desugars 1:1 into a one-element ops array, else the canonical
//! JSON source (`--edits-json` / `--ops-file` / stdin) is parsed — then serializes
//! the resolved [`EditOp`] array onto the wire [`EditParams`] and hands it to the
//! owner. The owner runs the pure transform against the target's current body and
//! — when `confirm` is set — writes the result under its single-writer lock,
//! answering with an [`EditReport`] the display layer renders (records / json).
//!
//! Apply-vs-forecast is the same client-side ladder as `set`/`new`: `--dry-run`
//! forecasts, `--yes` applies, everything else forecasts (a safe implicit
//! dry-run). A human at a TTY additionally gets the preview → prompt →
//! apply conversation, wired through [`run_confirm`] and
//! `display::emit_mutation`.
//!
//! # A CLI-side resolution error is a coded refusal, not a `norn:` diagnostic
//!
//! An op-resolution failure (a sugar conflict, malformed edits JSON, an empty
//! array) is rendered as `error: <message>` on stderr at exit 2 — edit's
//! refusal surface — so it is carried as a locally-built refused
//! [`EditReport`] rather than a [`Diagnostic`] (which would use the `norn:`
//! prefix and the operational exit). Every edit refusal — CLI-side or owner-side
//! preflight — thus renders through the one refusal branch in `render_edit`.

use std::io::Read;

use crate::cli::{EditArgs, GlobalArgs};
use crate::display::{Diagnostic, EditMutationView, Format, FormatChoice, FormatSpec, Output};
use norn_core::edit::ops::EditOp;
use norn_wire::{CodedError, EditParams, EditReport, MutationOutcome, EDIT_REPORT_SCHEMA_VERSION};

/// Run an `edit` and return its report as an [`Output`], or a soft-landing
/// [`Diagnostic`] on a connection/owner failure. A CLI-side op-resolution error
/// or an owner-side pre-write decline arrives as a report with `outcome =
/// refused` the display layer renders at exit 2.
pub fn run(args: &EditArgs, global: &GlobalArgs) -> Result<Output, Diagnostic> {
    run_confirm(args, global, args.mode.confirm())
}

/// Same as [`run`], but with `confirm` supplied rather than derived from
/// `args` — the dispatch loop's interactive retry (NRN-389) calls this
/// directly with `confirm: true` after a TTY 'y' answer. This is a SECOND
/// routed request, not a replay of the cached forecast: the owner re-runs the
/// transform and writes fresh under its lock, exactly as a direct `--yes`
/// invocation would.
pub(crate) fn run_confirm(
    args: &EditArgs,
    global: &GlobalArgs,
    confirm: bool,
) -> Result<Output, Diagnostic> {
    // Resolve the ops FIRST so a malformed input fails fast, before any owner
    // summon or write. A resolution error is a coded refusal (`error: <msg>`,
    // exit 2), built locally and returned as the Output.
    let ops = match resolve_ops(args) {
        Ok(ops) => ops,
        Err(msg) => return Ok(refused_output(args, &msg)),
    };

    // Re-serialize the resolved array onto the wire so the owner's transform runs
    // on the SAME ops (an `EditOp` has no non-string map keys or NaN floats, so
    // serialization is infallible).
    let edits = serde_json::to_string(&ops).expect("EditOp array serializes");
    let params = EditParams {
        target: args.target.clone(),
        edits,
        expected_hash: args.expected_hash.clone(),
        confirm,
    };

    let mut session = crate::routed::open_session(global)?;
    let report = session
        .edit(params)
        .map_err(|e| crate::routed::client_error_diagnostic(&e))?;

    Ok(edit_output(args, report))
}

/// Resolve the CLI ops: single-op sugar first, else the canonical JSON source
/// (`--edits-json` / `--ops-file` / stdin). Returns the bare error MESSAGE (no
/// `error:` prefix — the renderer adds it) on any resolution failure.
fn resolve_ops(args: &EditArgs) -> Result<Vec<EditOp>, String> {
    let ops = match desugar(args)? {
        Some(ops) => ops,
        None => {
            let raw = match (&args.edits_json, &args.ops_file) {
                (Some(s), _) => s.clone(),
                (None, Some(path)) => std::fs::read_to_string(path)
                    .map_err(|e| format!("failed to read ops file {path}: {e}"))?,
                (None, None) => {
                    let mut buf = String::new();
                    std::io::stdin()
                        .read_to_string(&mut buf)
                        .map_err(|e| format!("failed to read edits from stdin: {e}"))?;
                    buf
                }
            };
            serde_json::from_str(&raw).map_err(|e| format!("invalid edits JSON: {e}"))?
        }
    };
    if ops.is_empty() {
        return Err("edits array is empty".to_string());
    }
    Ok(ops)
}

/// The three payload flags a sugar op may carry.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Payload {
    New,
    Content,
    ReplaceAll,
}

/// Desugar the single-op flags into a one-element ops array.
/// `Ok(None)` — no op flag, fall back to the canonical
/// source; `Ok(Some(vec![op]))` — exactly one op flag with a valid payload;
/// `Err(msg)` — more than one op flag, an op flag combined with the canonical
/// source, or a missing/unconsumed payload.
fn desugar(args: &EditArgs) -> Result<Option<Vec<EditOp>>, String> {
    let mut present: Vec<&'static str> = Vec::new();
    if args.str_replace.is_some() {
        present.push("--str-replace");
    }
    if args.replace_section.is_some() {
        present.push("--replace-section");
    }
    if args.append_to_section.is_some() {
        present.push("--append-to-section");
    }
    if args.delete_section.is_some() {
        present.push("--delete-section");
    }
    if args.insert_before_heading.is_some() {
        present.push("--insert-before-heading");
    }
    if args.insert_after_heading.is_some() {
        present.push("--insert-after-heading");
    }

    if present.is_empty() {
        // No op-flag sugar: the payload flags (`--new`/`--content`/`--replace-all`)
        // belong to a sugar op; supplying one here would be silently dropped, so
        // refuse it (F3).
        reject_unconsumed_payloads(args, "this invocation", &[])?;
        return Ok(None);
    }
    if present.len() > 1 {
        return Err(format!(
            "at most one edit op flag may be given, got: {}",
            present.join(", ")
        ));
    }
    if args.edits_json.is_some() {
        return Err(format!(
            "{} cannot be combined with --edits-json",
            present[0]
        ));
    }
    if args.ops_file.is_some() {
        return Err(format!("{} cannot be combined with --ops-file", present[0]));
    }

    let op = if let Some(old) = &args.str_replace {
        reject_unconsumed_payloads(
            args,
            "str_replace",
            &[Payload::New, Payload::Content, Payload::ReplaceAll],
        )?;
        // `--content` is an accepted alias for `--new` on str_replace. Both with
        // DIFFERENT values is ambiguous — refuse (F2); same value is fine.
        let new = match (args.new.as_ref(), args.content.as_ref()) {
            (Some(n), Some(c)) if n != c => {
                return Err("conflicting payload: --new and --content differ".to_string());
            }
            (Some(n), _) => n,
            (None, Some(c)) => c,
            (None, None) => return Err("--str-replace requires --new (or --content)".to_string()),
        };
        EditOp::StrReplace {
            old: old.clone(),
            new: new.clone(),
            replace_all: args.replace_all,
        }
    } else if let Some(heading) = &args.replace_section {
        reject_unconsumed_payloads(args, "replace_section", &[Payload::Content])?;
        EditOp::ReplaceSection {
            heading: heading.clone(),
            content: require_content(args, "--replace-section")?,
        }
    } else if let Some(heading) = &args.append_to_section {
        reject_unconsumed_payloads(args, "append_to_section", &[Payload::Content])?;
        EditOp::AppendToSection {
            heading: heading.clone(),
            content: require_content(args, "--append-to-section")?,
        }
    } else if let Some(heading) = &args.delete_section {
        reject_unconsumed_payloads(args, "delete_section", &[])?;
        EditOp::DeleteSection {
            heading: heading.clone(),
        }
    } else if let Some(heading) = &args.insert_before_heading {
        reject_unconsumed_payloads(args, "insert_before_heading", &[Payload::Content])?;
        EditOp::InsertBeforeHeading {
            heading: heading.clone(),
            content: require_content(args, "--insert-before-heading")?,
        }
    } else if let Some(heading) = &args.insert_after_heading {
        reject_unconsumed_payloads(args, "insert_after_heading", &[Payload::Content])?;
        EditOp::InsertAfterHeading {
            heading: heading.clone(),
            content: require_content(args, "--insert-after-heading")?,
        }
    } else {
        unreachable!("present is non-empty and length 1");
    };

    Ok(Some(vec![op]))
}

/// Refuse any payload flag the selected op does not consume (F3).
fn reject_unconsumed_payloads(
    args: &EditArgs,
    target: &str,
    valid: &[Payload],
) -> Result<(), String> {
    if args.new.is_some() && !valid.contains(&Payload::New) {
        return Err(format!("flag --new does not apply to {target}"));
    }
    if args.content.is_some() && !valid.contains(&Payload::Content) {
        return Err(format!("flag --content does not apply to {target}"));
    }
    if args.replace_all && !valid.contains(&Payload::ReplaceAll) {
        return Err(format!("flag --replace-all does not apply to {target}"));
    }
    Ok(())
}

fn require_content(args: &EditArgs, flag: &str) -> Result<String, String> {
    args.content
        .clone()
        .ok_or_else(|| format!("{flag} requires --content"))
}

/// Build the `Output` for a successful edit report (forecast / applied).
fn edit_output(args: &EditArgs, report: EditReport) -> Output {
    Output::Edit(EditMutationView {
        report,
        format: FormatChoice {
            explicit: Some(args.mode.format.into()),
            // Mutations do not switch format on isatty — the mode ladder decides
            // apply-vs-forecast, and `--format` (default records) decides shape.
            spec: FormatSpec {
                tty: Format::Records,
                piped: Format::Records,
            },
        },
    })
}

/// Build a locally-refused edit `Output` for a CLI-side op-resolution failure
/// (rendered as `error: <message>` on stderr, exit 2).
fn refused_output(args: &EditArgs, message: &str) -> Output {
    let report = EditReport {
        schema_version: EDIT_REPORT_SCHEMA_VERSION,
        trace_id: String::new(),
        operation: "edit".into(),
        target: args.target.clone(),
        edits: Vec::new(),
        body_changed: false,
        body_bytes_old: None,
        body_bytes_new: None,
        applied: false,
        outcome: MutationOutcome::Refused,
        error: Some(CodedError {
            code: "edit-input-invalid".into(),
            message: message.to_string(),
            path: None,
        }),
    };
    edit_output(args, report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use clap::Parser;

    fn edit_args(argv: &[&str]) -> EditArgs {
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Edit(a) => a,
            other => panic!("expected edit, got {other:?}"),
        }
    }

    #[test]
    fn str_replace_sugar_desugars() {
        let args = edit_args(&[
            "norn",
            "edit",
            "a.md",
            "--str-replace",
            "old",
            "--new",
            "new",
        ]);
        let ops = resolve_ops(&args).unwrap();
        assert_eq!(
            ops,
            vec![EditOp::StrReplace {
                old: "old".into(),
                new: "new".into(),
                replace_all: false
            }]
        );
    }

    #[test]
    fn content_aliases_new_on_str_replace() {
        let args = edit_args(&[
            "norn",
            "edit",
            "a.md",
            "--str-replace",
            "old",
            "--content",
            "new",
        ]);
        let ops = resolve_ops(&args).unwrap();
        assert_eq!(
            ops,
            vec![EditOp::StrReplace {
                old: "old".into(),
                new: "new".into(),
                replace_all: false
            }]
        );
    }

    #[test]
    fn section_sugar_desugars() {
        let args = edit_args(&[
            "norn",
            "edit",
            "a.md",
            "--append-to-section",
            "Tasks",
            "--content",
            "line one",
        ]);
        let ops = resolve_ops(&args).unwrap();
        assert_eq!(
            ops,
            vec![EditOp::AppendToSection {
                heading: "Tasks".into(),
                content: "line one".into()
            }]
        );
    }

    #[test]
    fn delete_section_needs_no_content() {
        let args = edit_args(&["norn", "edit", "a.md", "--delete-section", "Tasks"]);
        let ops = resolve_ops(&args).unwrap();
        assert_eq!(
            ops,
            vec![EditOp::DeleteSection {
                heading: "Tasks".into()
            }]
        );
    }

    #[test]
    fn edits_json_parses() {
        let args = edit_args(&[
            "norn",
            "edit",
            "a.md",
            "--edits-json",
            r#"[{"op":"str_replace","old":"a","new":"b"}]"#,
        ]);
        let ops = resolve_ops(&args).unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].kind(), "str_replace");
    }

    #[test]
    fn two_op_flags_error() {
        let args = edit_args(&[
            "norn",
            "edit",
            "a.md",
            "--str-replace",
            "old",
            "--new",
            "new",
            "--delete-section",
            "Tasks",
        ]);
        assert!(resolve_ops(&args).is_err());
    }

    #[test]
    fn op_flag_with_edits_json_errors() {
        let args = edit_args(&[
            "norn",
            "edit",
            "a.md",
            "--str-replace",
            "old",
            "--new",
            "new",
            "--edits-json",
            "[]",
        ]);
        assert!(resolve_ops(&args).is_err());
    }

    #[test]
    fn str_replace_without_new_errors() {
        let args = edit_args(&["norn", "edit", "a.md", "--str-replace", "old"]);
        let err = resolve_ops(&args).unwrap_err();
        assert!(err.contains("--str-replace requires --new"), "{err}");
    }

    #[test]
    fn new_and_content_differ_errors() {
        let args = edit_args(&[
            "norn",
            "edit",
            "a.md",
            "--str-replace",
            "old",
            "--new",
            "A",
            "--content",
            "B",
        ]);
        let err = resolve_ops(&args).unwrap_err();
        assert!(err.contains("differ"), "{err}");
    }

    #[test]
    fn delete_section_with_content_errors() {
        let args = edit_args(&[
            "norn",
            "edit",
            "a.md",
            "--delete-section",
            "H",
            "--content",
            "X",
        ]);
        let err = resolve_ops(&args).unwrap_err();
        assert!(
            err.contains("--content") && err.contains("delete_section"),
            "{err}"
        );
    }

    #[test]
    fn empty_edits_json_is_empty_array_error() {
        let args = edit_args(&["norn", "edit", "a.md", "--edits-json", "[]"]);
        let err = resolve_ops(&args).unwrap_err();
        assert_eq!(err, "edits array is empty");
    }

    #[test]
    fn malformed_op_is_invalid_json_error() {
        let args = edit_args(&["norn", "edit", "a.md", "--edits-json", r#"[{"op":"nope"}]"#]);
        let err = resolve_ops(&args).unwrap_err();
        assert!(err.starts_with("invalid edits JSON:"), "{err}");
    }
}
