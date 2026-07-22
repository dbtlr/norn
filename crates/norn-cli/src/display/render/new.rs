//! `new` (mutation report) (NRN-409).

use std::io::{self, Write};

use norn_wire::MutationOutcome;

use crate::cli::GlobalArgs;
use crate::display::conversation::Conversation;
use crate::display::emit::{is_stdout_tty, render_outcome};
use crate::display::format::Format;
use crate::display::output::NewMutationView;
use crate::display::{Presenter, EXIT_OK};

use super::shared::{mutation_exit, value_repr, warning_short};

/// A `new`-report key/value line: `{label:<9}  {value}` (value column at 11), the
/// donor's aligned block. Unstyled (like `count` / `describe`) so the piped /
/// parity bytes are exact.
fn new_kv(out: &mut dyn Write, label: &str, value: &str) -> io::Result<()> {
    writeln!(out, "{label:<9}  {value}")
}

/// The provenance detail for a created field (donor `report.rs`): the source
/// label as the core already set it (`operator-flag` / `operator-flag-json` /
/// `schema-default` — one vocabulary, no remap), plus the crediting rule when a
/// default carried one: `schema-default, task-rule`.
fn created_detail(created: &norn_wire::FrontmatterCreated) -> String {
    match &created.rule {
        Some(rule) => format!("{}, {}", created.source, rule),
        None => created.source.clone(),
    }
}

pub(crate) fn render_new<O: Write, E: Write>(
    view: NewMutationView,
    global: &GlobalArgs,
    presenter: &mut Presenter<O, E>,
) -> i32 {
    let format = view.spec.resolve(view.explicit, is_stdout_tty());
    let report = &view.report;
    let (out, err) = presenter.streams();
    let mut conv = Conversation::new(err);

    // JSON: pretty-printed with ALPHABETICAL keys (the donor serialized through a
    // `serde_json::Value`, whose map is a BTreeMap without `preserve_order`).
    if format == Format::Json {
        let _ = global;
        let result: io::Result<i32> = (|| {
            let value = serde_json::to_value(report)?;
            // The donor's `new --format json` emits NO trailing newline (unlike
            // `set --format json`, which does) — a known cross-verb JSON framing
            // inconsistency, unified by NRN-408 (one trailing-newline rule);
            // current behavior held here (`write!`, not `writeln!`) until then.
            write!(out, "{}", serde_json::to_string_pretty(&value)?)?;
            Ok(mutation_exit(report.outcome))
        })();
        return render_outcome(result, conv.writer());
    }

    if report.outcome == MutationOutcome::Refused {
        let msg = report
            .error
            .as_ref()
            .map(|e| e.message.clone())
            .unwrap_or_else(|| "new refused".to_string());
        let result: io::Result<i32> = (|| {
            conv.line(&format!("error: {msg}"))?;
            Ok(crate::display::EXIT_USAGE)
        })();
        return render_outcome(result, conv.writer());
    }

    let result: io::Result<i32> = (|| {
        let shown_path = report
            .path
            .as_deref()
            .or(report.predicted_path.as_deref())
            .unwrap_or("(pending)");
        new_kv(out, "path", shown_path)?;
        new_kv(out, "operation", "new")?;
        new_kv(out, "applied", &report.applied.to_string())?;

        // fields: `none`, or the first created field on the `fields` line with the
        // rest aligned under the value column (11-space continuation indent).
        if report.frontmatter_created.is_empty() {
            new_kv(out, "fields", "none")?;
        } else {
            // Field-name sub-column padding (donor report.rs max_field_w): every
            // `field` cell is left-padded to the widest name so the `=` aligns.
            let field_w = report
                .frontmatter_created
                .iter()
                .map(|c| c.field.len())
                .max()
                .unwrap_or(0);
            for (i, created) in report.frontmatter_created.iter().enumerate() {
                let line = format!(
                    "{:<field_w$} = {}  ({})",
                    created.field,
                    value_repr(&created.value),
                    created_detail(created)
                );
                if i == 0 {
                    new_kv(out, "fields", &line)?;
                } else {
                    writeln!(out, "           {line}")?;
                }
            }
        }

        new_kv(out, "body", &format!("{} bytes", report.body_bytes))?;

        if report.warnings.is_empty() {
            new_kv(out, "warnings", "none")?;
        } else {
            for (i, w) in report.warnings.iter().enumerate() {
                if i == 0 {
                    new_kv(out, "warnings", &warning_short(w))?;
                } else {
                    writeln!(out, "           {}", warning_short(w))?;
                }
            }
        }

        if report.applied {
            writeln!(out, "trace: {}", report.trace_id)?;
        } else {
            writeln!(out)?;
            writeln!(out, "Apply with --yes")?;
        }
        Ok(EXIT_OK)
    })();
    render_outcome(result, conv.writer())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ColorWhen;
    use crate::display::format::FormatSpec;
    use norn_wire::{FrontmatterCreated, NewReport};
    use serde_json::json;

    fn global_args() -> GlobalArgs {
        GlobalArgs {
            cwd: None,
            verbose: false,
            no_cache_refresh: false,
            color: ColorWhen::Never,
            vault: None,
            help_short: false,
            help_long: false,
            dynamic_fields: Vec::new(),
        }
    }

    fn created(
        field: &str,
        value: serde_json::Value,
        source: &str,
        rule: Option<&str>,
    ) -> FrontmatterCreated {
        FrontmatterCreated {
            field: field.into(),
            value,
            source: source.into(),
            rule: rule.map(str::to_string),
        }
    }

    #[test]
    fn f1_created_detail_uses_the_core_vocabulary_verbatim() {
        // No remap layer: the source label is whatever the core set, plus the
        // crediting rule when a default carried one.
        assert_eq!(
            created_detail(&created(
                "type",
                json!("note"),
                "schema-default",
                Some("typed-note")
            )),
            "schema-default, typed-note"
        );
        assert_eq!(
            created_detail(&created("title", json!("X"), "operator-flag", None)),
            "operator-flag"
        );
        assert_eq!(
            created_detail(&created("tags", json!([1]), "operator-flag-json", None)),
            "operator-flag-json"
        );
    }

    #[test]
    fn f3_new_records_pads_field_names_to_the_widest() {
        let report = NewReport {
            schema_version: 2,
            trace_id: String::new(),
            operation: "new".into(),
            path: Some("a.md".into()),
            applied: false,
            outcome: MutationOutcome::Applied,
            frontmatter_created: vec![
                created("kind", json!("note"), "schema-default", Some("r")),
                created("verylongfield", json!("v"), "operator-flag", None),
            ],
            body_bytes: 0,
            warnings: vec![],
            predicted_path: None,
            error: None,
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut presenter = Presenter::new(&mut out, &mut err);
            render_new(
                NewMutationView {
                    report,
                    explicit: Some(Format::Records),
                    spec: FormatSpec {
                        tty: Format::Records,
                        piped: Format::Records,
                    },
                },
                &global_args(),
                &mut presenter,
            );
        }
        let s = String::from_utf8(out).unwrap();
        // Both field cells are padded to the widest name (13 chars), so the `=`
        // columns align.
        assert!(s.contains("kind          = note"), "{s}");
        assert!(s.contains("verylongfield = v"), "{s}");
    }
}
