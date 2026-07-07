//! `norn edit` single-op sugar (ADR 0010, NRN-210).
//!
//! Canonical `edit` input stays the JSON ops array (`--edits-json` / `--ops-file`
//! / stdin). For the common ONE-op case this module desugars a per-op flag into a
//! one-element ops array that flows through the EXACT same apply path — the flag
//! IS the op (kebab of the op vocabulary), its value is the op's ANCHOR, and the
//! companion flags (`--new`, `--content`, `--replace-all`) are the op's payload
//! fields named exactly as the JSON fields.
//!
//! Exactly ONE op flag per invocation. Two op flags, or an op flag together with
//! `--edits-json` / `--ops-file` / stdin, is a hard error (mutually exclusive).

use anyhow::{bail, Result};

use crate::cli::EditArgs;
use crate::edit::ops::EditOp;

/// Desugar the single-op flags on `EditArgs` into a one-element ops array.
///
/// Returns:
/// - `Ok(None)` — no op flag present; the caller falls back to the canonical
///   `--edits-json` / `--ops-file` / stdin source.
/// - `Ok(Some(vec![op]))` — exactly one op flag present and its payload valid.
/// - `Err` — more than one op flag, an op flag combined with the canonical
///   source (`--edits-json` / `--ops-file`), or a missing required payload.
pub fn desugar(args: &EditArgs) -> Result<Option<Vec<EditOp>>> {
    // Enumerate which op flags are set, preserving their canonical names for
    // the multi-op error message.
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
        // No op-flag sugar: the canonical `--edits-json` / `--ops-file` / stdin
        // path carries the ops. The payload flags (`--new` / `--content` /
        // `--replace-all`) belong to a sugar op; supplying one here would be
        // silently dropped, so refuse it (F3).
        reject_unconsumed_payloads(args, None, &[])?;
        return Ok(None);
    }
    if present.len() > 1 {
        bail!(
            "at most one edit op flag may be given, got: {}",
            present.join(", ")
        );
    }
    if args.edits_json.is_some() {
        bail!("{} cannot be combined with --edits-json", present[0]);
    }
    if args.ops_file.is_some() {
        bail!("{} cannot be combined with --ops-file", present[0]);
    }

    let op = if let Some(old) = &args.str_replace {
        // str_replace consumes every payload flag: `--content` is an accepted
        // alias for `--new`, and `--replace-all` applies.
        reject_unconsumed_payloads(
            args,
            Some("str_replace"),
            &[Payload::New, Payload::Content, Payload::ReplaceAll],
        )?;
        // `--content` is an accepted alias for `--new` on str_replace. Supplying
        // BOTH with DIFFERENT values is ambiguous — refuse rather than silently
        // letting one win (F2). Both present with the SAME value is fine.
        let new = match (args.new.as_ref(), args.content.as_ref()) {
            (Some(n), Some(c)) if n != c => {
                bail!("conflicting payload: --new and --content differ");
            }
            (Some(n), _) => n,
            (None, Some(c)) => c,
            (None, None) => {
                bail!("--str-replace requires --new (or --content)")
            }
        };
        EditOp::StrReplace {
            old: old.clone(),
            new: new.clone(),
            replace_all: args.replace_all,
        }
    } else if let Some(heading) = &args.replace_section {
        reject_unconsumed_payloads(args, Some("replace_section"), &[Payload::Content])?;
        EditOp::ReplaceSection {
            heading: heading.clone(),
            content: require_content(args, "--replace-section")?,
        }
    } else if let Some(heading) = &args.append_to_section {
        reject_unconsumed_payloads(args, Some("append_to_section"), &[Payload::Content])?;
        EditOp::AppendToSection {
            heading: heading.clone(),
            content: require_content(args, "--append-to-section")?,
        }
    } else if let Some(heading) = &args.delete_section {
        // delete_section takes no payload at all.
        reject_unconsumed_payloads(args, Some("delete_section"), &[])?;
        EditOp::DeleteSection {
            heading: heading.clone(),
        }
    } else if let Some(heading) = &args.insert_before_heading {
        reject_unconsumed_payloads(args, Some("insert_before_heading"), &[Payload::Content])?;
        EditOp::InsertBeforeHeading {
            heading: heading.clone(),
            content: require_content(args, "--insert-before-heading")?,
        }
    } else if let Some(heading) = &args.insert_after_heading {
        reject_unconsumed_payloads(args, Some("insert_after_heading"), &[Payload::Content])?;
        EditOp::InsertAfterHeading {
            heading: heading.clone(),
            content: require_content(args, "--insert-after-heading")?,
        }
    } else {
        unreachable!("present is non-empty and length 1");
    };

    Ok(Some(vec![op]))
}

/// The three payload flags a sugar op may carry.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Payload {
    New,
    Content,
    ReplaceAll,
}

/// Refuse any payload flag (`--new` / `--content` / `--replace-all`) that the
/// selected op does not consume (F3). `op` names the selected op for the error
/// (None = no op-flag sugar, i.e. the canonical `--edits-json`/`--ops-file`/
/// stdin path); `valid` enumerates the payloads the op accepts. Any payload set
/// but absent from `valid` is a hard error — never a silent drop.
fn reject_unconsumed_payloads(args: &EditArgs, op: Option<&str>, valid: &[Payload]) -> Result<()> {
    let target = op.unwrap_or("this invocation");
    if args.new.is_some() && !valid.contains(&Payload::New) {
        bail!("flag --new does not apply to {target}");
    }
    if args.content.is_some() && !valid.contains(&Payload::Content) {
        bail!("flag --content does not apply to {target}");
    }
    if args.replace_all && !valid.contains(&Payload::ReplaceAll) {
        bail!("flag --replace-all does not apply to {target}");
    }
    Ok(())
}

fn require_content(args: &EditArgs, flag: &str) -> Result<String> {
    args.content
        .clone()
        .ok_or_else(|| anyhow::anyhow!("{flag} requires --content"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_args() -> EditArgs {
        EditArgs {
            target: "note.md".into(),
            edits_json: None,
            ops_file: None,
            str_replace: None,
            replace_section: None,
            append_to_section: None,
            delete_section: None,
            insert_before_heading: None,
            insert_after_heading: None,
            new: None,
            content: None,
            replace_all: false,
            expected_hash: None,
            yes: false,
            dry_run: false,
            format: crate::cli::EditFormat::Json,
        }
    }

    #[test]
    fn no_op_flag_returns_none() {
        assert!(desugar(&base_args()).unwrap().is_none());
    }

    #[test]
    fn str_replace_with_new_desugars() {
        let mut a = base_args();
        a.str_replace = Some("old".into());
        a.new = Some("new".into());
        let ops = desugar(&a).unwrap().unwrap();
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
    fn str_replace_accepts_content_alias_for_new() {
        let mut a = base_args();
        a.str_replace = Some("old".into());
        a.content = Some("new".into());
        let ops = desugar(&a).unwrap().unwrap();
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
    fn str_replace_all_flag_flows_through() {
        let mut a = base_args();
        a.str_replace = Some("old".into());
        a.new = Some("new".into());
        a.replace_all = true;
        let ops = desugar(&a).unwrap().unwrap();
        assert_eq!(
            ops,
            vec![EditOp::StrReplace {
                old: "old".into(),
                new: "new".into(),
                replace_all: true
            }]
        );
    }

    #[test]
    fn str_replace_without_new_errors() {
        let mut a = base_args();
        a.str_replace = Some("old".into());
        assert!(desugar(&a).is_err());
    }

    #[test]
    fn section_ops_desugar() {
        let mut a = base_args();
        a.replace_section = Some("Tasks".into());
        a.content = Some("- x".into());
        assert_eq!(
            desugar(&a).unwrap().unwrap(),
            vec![EditOp::ReplaceSection {
                heading: "Tasks".into(),
                content: "- x".into()
            }]
        );
    }

    #[test]
    fn delete_section_needs_no_content() {
        let mut a = base_args();
        a.delete_section = Some("Tasks".into());
        assert_eq!(
            desugar(&a).unwrap().unwrap(),
            vec![EditOp::DeleteSection {
                heading: "Tasks".into()
            }]
        );
    }

    #[test]
    fn section_op_without_content_errors() {
        let mut a = base_args();
        a.append_to_section = Some("Tasks".into());
        assert!(desugar(&a).is_err());
    }

    #[test]
    fn two_op_flags_error() {
        let mut a = base_args();
        a.str_replace = Some("old".into());
        a.new = Some("new".into());
        a.delete_section = Some("Tasks".into());
        assert!(desugar(&a).is_err());
    }

    #[test]
    fn op_flag_with_edits_json_errors() {
        let mut a = base_args();
        a.str_replace = Some("old".into());
        a.new = Some("new".into());
        a.edits_json = Some("[]".into());
        assert!(desugar(&a).is_err());
    }

    #[test]
    fn op_flag_with_ops_file_errors() {
        let mut a = base_args();
        a.delete_section = Some("Tasks".into());
        a.ops_file = Some("ops.json".into());
        assert!(desugar(&a).is_err());
    }

    // ── F2: --new / --content conflict on str_replace ────────────────────────

    #[test]
    fn str_replace_new_and_content_differ_errors() {
        let mut a = base_args();
        a.str_replace = Some("old".into());
        a.new = Some("AAA".into());
        a.content = Some("BBB".into());
        let err = desugar(&a).unwrap_err().to_string();
        assert!(
            err.contains("--new") && err.contains("--content") && err.contains("differ"),
            "expected conflicting-payload error, got: {err}"
        );
    }

    #[test]
    fn str_replace_new_and_content_same_value_ok() {
        let mut a = base_args();
        a.str_replace = Some("old".into());
        a.new = Some("AAA".into());
        a.content = Some("AAA".into());
        let ops = desugar(&a).unwrap().unwrap();
        assert_eq!(
            ops,
            vec![EditOp::StrReplace {
                old: "old".into(),
                new: "AAA".into(),
                replace_all: false
            }]
        );
    }

    // ── F3: unconsumed payload flags are a hard error, never silently dropped ─

    #[test]
    fn delete_section_with_content_errors() {
        let mut a = base_args();
        a.delete_section = Some("H".into());
        a.content = Some("X".into());
        let err = desugar(&a).unwrap_err().to_string();
        assert!(
            err.contains("--content") && err.contains("delete_section"),
            "expected unconsumed-payload error, got: {err}"
        );
    }

    #[test]
    fn delete_section_with_new_errors() {
        let mut a = base_args();
        a.delete_section = Some("H".into());
        a.new = Some("X".into());
        assert!(desugar(&a).is_err());
    }

    #[test]
    fn delete_section_with_replace_all_errors() {
        let mut a = base_args();
        a.delete_section = Some("H".into());
        a.replace_all = true;
        assert!(desugar(&a).is_err());
    }

    #[test]
    fn replace_section_with_replace_all_errors() {
        let mut a = base_args();
        a.replace_section = Some("H".into());
        a.content = Some("C".into());
        a.replace_all = true;
        let err = desugar(&a).unwrap_err().to_string();
        assert!(
            err.contains("--replace-all") && err.contains("replace_section"),
            "expected unconsumed-payload error, got: {err}"
        );
    }

    #[test]
    fn section_op_with_new_errors() {
        let mut a = base_args();
        a.append_to_section = Some("H".into());
        a.content = Some("C".into());
        a.new = Some("X".into());
        assert!(desugar(&a).is_err());
    }

    #[test]
    fn payload_without_op_flag_errors() {
        // --new / --content / --replace-all supplied with NO op flag (the
        // canonical --edits-json/--ops-file/stdin path) would be silently
        // dropped; refuse each.
        let mut a = base_args();
        a.new = Some("X".into());
        assert!(desugar(&a).is_err());

        let mut b = base_args();
        b.content = Some("X".into());
        assert!(desugar(&b).is_err());

        let mut c = base_args();
        c.replace_all = true;
        assert!(desugar(&c).is_err());
    }

    #[test]
    fn payload_with_edits_json_and_no_op_flag_errors() {
        let mut a = base_args();
        a.edits_json = Some("[]".into());
        a.content = Some("X".into());
        let err = desugar(&a).unwrap_err().to_string();
        assert!(
            err.contains("--content") && err.contains("this invocation"),
            "expected unconsumed-payload error, got: {err}"
        );
    }
}
