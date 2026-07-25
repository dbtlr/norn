//! `describe` (NRN-409).
//!
//! Two payloads, each rendered in both slots so `--format json` carries exactly
//! the content the same invocation prints without it (NRN-416/NRN-470): the
//! default SUMMARY (structure counts + the optional contents-summary) and, under
//! `--schema`, the full declared config (every folder, every rule with its
//! defaults, the frontmatter schema).

use std::fmt::Write as _;
use std::io;

use norn_core::standards::ValidateConfig;
use norn_wire::{DataSummary, DescribeReport};
use serde::Serialize;
use serde_json::Value;

use crate::display::conversation::Conversation;
use crate::display::emit::render_outcome;
use crate::display::format::Format;
use crate::display::output::DescribeView;
use crate::display::sink::Sink;
use crate::display::EXIT_OK;

pub(crate) fn render_describe(
    view: DescribeView,
    format: Format,
    sink: &mut Sink<'_>,
    conv: &mut Conversation<'_>,
) -> i32 {
    let text = match (view.schema, format) {
        (true, Format::Json) => schema_json(&view.report),
        (true, _) => schema_text(&view.report),
        (false, Format::Json) => summary_json(&view.report),
        (false, _) => summary_text(&view.report),
    };
    let result: io::Result<i32> = (|| {
        if text.ends_with('\n') {
            write!(sink.writer(), "{text}")?;
        } else {
            writeln!(sink.writer(), "{text}")?;
        }
        warn_unknown_by_describe(&view.report, &view.by, conv)?;
        Ok(EXIT_OK)
    })();
    render_outcome(result, conv.writer())
}

/// `describe`'s counterpart to `warn_unknown_by_count` (NRN-374). Unlike
/// `count`, describe's `field_distributions` (`norn-core`) drops a `--by`
/// field ENTIRELY from `data.fields` when it has zero occurrences across the
/// matched set (no `(missing)` bucket is even synthesized for an explicit
/// `--by`, unlike `count`'s always-present bucket) — so "absent" here means
/// "requested but not in `data.fields`" rather than an all-`(missing)` bucket.
/// `by` is trimmed the same way `describe::execute`'s internal `normalize_by`
/// does (never de-duped, matching it), so a comma/whitespace-only entry never
/// warns. Skipped entirely on `data: None` (no `--data`/`--by` requested) and
/// on a zero-match `data.total` (every field would trivially be absent,
/// redundant with the `0 documents` line).
fn warn_unknown_by_describe(
    report: &DescribeReport,
    by: &[String],
    conv: &mut Conversation<'_>,
) -> io::Result<()> {
    let Some(data) = &report.data else {
        return Ok(());
    };
    if data.total == 0 {
        return Ok(());
    }
    for field in by.iter().map(|f| f.trim()).filter(|f| !f.is_empty()) {
        let present = data.fields.iter().any(|fd| fd.field == field);
        if !present {
            conv.warning(&format!(
                "--by field `{field}` not present in any matching document"
            ))?;
        }
    }
    Ok(())
}

/// The records rendering of an unconfigured `inbox.path`, mirroring the `null`
/// the json slot carries.
const NO_INBOX: &str = "(none)";

/// The default payload: the structure COUNTS, the inbox target, and the optional
/// contents-summary. Both slots derive from this one struct — [`summary_json`]
/// serializes it and [`summary_text`] destructures it — so a field added here
/// fails to compile until the records formatter accounts for it.
#[derive(Serialize)]
struct DescribeSummary<'a> {
    folders: usize,
    path_rules: usize,
    creatable_rules: usize,
    inbox: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<&'a DataSummary>,
}

fn summary(report: &DescribeReport) -> DescribeSummary<'_> {
    DescribeSummary {
        folders: report.folders.len(),
        path_rules: report.path_rules.len(),
        creatable_rules: report.creatable_rules.len(),
        inbox: report.inbox.as_deref(),
        data: report.data.as_ref(),
    }
}

fn summary_json(report: &DescribeReport) -> String {
    serde_json::to_string(&summary(report)).unwrap_or_else(|_| "{}".to_string())
}

/// Every count line is printed, zeros included: in a counts block `0` is the
/// answer, and printing all four keeps the records block a line-for-line mirror
/// of the json object.
fn summary_text(report: &DescribeReport) -> String {
    let DescribeSummary {
        folders,
        path_rules,
        creatable_rules,
        inbox,
        data,
    } = summary(report);
    let mut s = String::new();
    let _ = writeln!(s, "folders    {folders}");
    let _ = writeln!(s, "path rules {path_rules}");
    let _ = writeln!(s, "creatable  {creatable_rules}");
    let _ = writeln!(s, "inbox      {}", inbox.unwrap_or(NO_INBOX));
    if let Some(data) = data {
        append_data_block(&mut s, data);
    }
    s
}

fn schema_json(report: &DescribeReport) -> String {
    serde_json::to_string(report).unwrap_or_else(|_| "{}".to_string())
}

/// The declared config in full, section per report field: the folder list, each
/// path/creatable rule with its frontmatter defaults, the inbox target, and the
/// frontmatter schema — the same content [`schema_json`] serializes. A section
/// the vault declares nothing for is omitted: unlike the counts block, where `0`
/// is an answer, an empty enumeration has nothing to show.
fn schema_text(report: &DescribeReport) -> String {
    let mut s = String::new();
    if !report.folders.is_empty() {
        let _ = writeln!(s, "folders");
        for folder in &report.folders {
            let name = if folder.is_empty() { "(root)" } else { folder };
            let _ = writeln!(s, "  {name}");
        }
    }
    if !report.path_rules.is_empty() {
        section_break(&mut s);
        let _ = writeln!(s, "path rules");
        for rule in &report.path_rules {
            match &rule.name {
                Some(name) => {
                    let _ = writeln!(s, "  {} ({})", rule.glob, name);
                }
                None => {
                    let _ = writeln!(s, "  {}", rule.glob);
                }
            }
            append_defaults(&mut s, &rule.frontmatter_defaults);
        }
    }
    if !report.creatable_rules.is_empty() {
        section_break(&mut s);
        let _ = writeln!(s, "creatable rules");
        for rule in &report.creatable_rules {
            let _ = writeln!(s, "  {} → {}", rule.name, rule.target);
            if !rule.required_vars.is_empty() {
                let _ = writeln!(s, "    required vars: {}", rule.required_vars.join(", "));
            }
            append_defaults(&mut s, &rule.frontmatter_defaults);
            if let Some(body) = &rule.body {
                let _ = writeln!(s, "    body: {} lines", body.lines().count());
            }
        }
    }
    if let Some(inbox) = &report.inbox {
        section_break(&mut s);
        let _ = writeln!(s, "inbox");
        let _ = writeln!(s, "  {inbox}");
    }
    if let Some(schema) = declared_schema(report) {
        section_break(&mut s);
        let _ = writeln!(s, "schema");
        let _ = writeln!(s, "{}", yaml_block(schema, 2));
    }
    if let Some(data) = &report.data {
        append_data_block(&mut s, data);
    }
    s
}

/// The frontmatter schema when the vault declares one. `describe` serializes a
/// default [`ValidateConfig`] for a vault running under no config file, and a
/// config declaring no `validate` section serializes to the same value, so an
/// all-empty schema means "nothing declared" and its section is dropped rather
/// than printed as three empty keys. A config that spells out an empty
/// `validate:` block collapses into that same case, deliberately: it enforces
/// exactly the rules a config with no `validate` section enforces — none — so
/// the two are one state to a reader of the records dump. `--format json` still
/// carries the `schema` key either way.
fn declared_schema(report: &DescribeReport) -> Option<&Value> {
    let undeclared = serde_json::to_value(ValidateConfig::default()).unwrap_or(Value::Null);
    (!is_empty_value(&report.schema) && report.schema != undeclared).then_some(&report.schema)
}

/// The contents-summary block (`--data`/`--stats`/`--by`), shared by both
/// records payloads: totals + date bounds, one line per distributed field, and
/// the identity-skipped tail.
fn append_data_block(s: &mut String, data: &DataSummary) {
    section_break(s);
    let dates = data
        .dates
        .iter()
        .map(|d| format!("{} {} → {}", d.field, d.min, d.max))
        .collect::<Vec<_>>()
        .join(" · ");
    if dates.is_empty() {
        let _ = writeln!(s, "{} documents", data.total);
    } else {
        let _ = writeln!(s, "{} documents · {}", data.total, dates);
    }
    let label_width = data.fields.iter().map(|f| f.field.len()).max().unwrap_or(0) + 1;
    for f in &data.fields {
        let body = f
            .values
            .iter()
            .map(|vc| format!("{} {}", vc.value, vc.count))
            .collect::<Vec<_>>()
            .join(" · ");
        let more = if f.more > 0 {
            format!(" (+{} more)", f.more)
        } else {
            String::new()
        };
        let _ = writeln!(s, "{:<label_width$} {body}{more}", format!("{}:", f.field));
    }
    if !data.skipped.is_empty() {
        let sk = data
            .skipped
            .iter()
            .map(|sf| format!("{} {}/{}", sf.field, sf.distinct, sf.total))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(s, "(skipped: {sk})");
    }
}

/// A rule's `frontmatter_defaults`, indented under the rule that declares them.
/// Omitted when the rule declares none.
fn append_defaults(s: &mut String, defaults: &Value) {
    if is_empty_value(defaults) {
        return;
    }
    let _ = writeln!(s, "    defaults:");
    let _ = writeln!(s, "{}", yaml_block(defaults, 6));
}

/// Blank-line separator before a section, skipped at the start of the output.
fn section_break(s: &mut String) {
    if !s.is_empty() {
        let _ = writeln!(s);
    }
}

fn is_empty_value(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Object(map) => map.is_empty(),
        Value::Array(items) => items.is_empty(),
        _ => false,
    }
}

/// A config payload rendered as YAML, every line indented by `indent` spaces.
/// Rule defaults and the frontmatter schema are arbitrary nested maps with no
/// flat key/value rendering, so records prints them in the same syntax
/// `.norn/config.yaml` declares them in. The returned block carries no trailing
/// newline.
fn yaml_block(value: &Value, indent: usize) -> String {
    let yaml = serde_yaml::to_string(value).unwrap_or_default();
    let pad = " ".repeat(indent);
    yaml.lines()
        .map(|line| {
            if line.is_empty() {
                String::new()
            } else {
                format!("{pad}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::format::{FormatChoice, FormatSpec};
    use crate::display::Presenter;
    use crate::display::EXIT_OPERATIONAL;
    use crate::output::palette::Palette;
    use crate::test_support::FailingWriter;
    use norn_wire::{
        CreatableRule, DateBounds, FieldDistribution, PathRule, SkippedField, ValueCount,
    };
    use serde_json::json;
    use std::io::Write;

    /// Drive `render_describe` through the same resolution `emit` performs —
    /// describe is unstyled, so a no-op palette sink.
    fn drive<O: Write, E: Write>(view: DescribeView, presenter: &mut Presenter<O, E>) -> i32 {
        let format = view.format.resolve(false);
        let palette = Palette::off();
        let (out, err) = presenter.streams();
        let mut sink = Sink::new(out, &palette, 80);
        let mut conv = Conversation::new(err);
        render_describe(view, format, &mut sink, &mut conv)
    }

    /// The frontmatter schema a config-less vault reports: `describe::execute`
    /// serializes a default `ValidateConfig` when no config file is in play.
    fn undeclared_schema() -> Value {
        serde_json::to_value(ValidateConfig::default()).unwrap()
    }

    /// A frontmatter schema a configured vault reports, round-tripped through
    /// `ValidateConfig` so the fixture carries the same key set `execute`
    /// serializes (every rule field defaulted and present).
    fn declared_schema_value() -> Value {
        let config: ValidateConfig = serde_json::from_value(json!({
            "required_frontmatter": ["type"],
            "rules": [{ "name": "note", "required_frontmatter": ["type"] }],
        }))
        .expect("the fixture parses as a validate config");
        serde_json::to_value(&config).expect("a validate config serializes")
    }

    /// A config-less vault with documents: folders only, no rules, no inbox, and
    /// the default (undeclared) schema — the shape `execute` builds for a vault
    /// running under no config file.
    fn describe_sample() -> DescribeReport {
        DescribeReport {
            folders: vec!["".into(), "notes".into()],
            path_rules: vec![],
            creatable_rules: vec![],
            inbox: None,
            schema: undeclared_schema(),
            data: Some(DataSummary {
                total: 1164,
                fields: vec![FieldDistribution {
                    field: "type".into(),
                    values: vec![
                        ValueCount {
                            value: "note".into(),
                            count: 575,
                        },
                        ValueCount {
                            value: "task".into(),
                            count: 420,
                        },
                    ],
                    more: 4,
                }],
                dates: vec![DateBounds {
                    field: "created".into(),
                    min: "2026-05-10".into(),
                    max: "2026-07-03".into(),
                }],
                skipped: vec![SkippedField {
                    field: "title".into(),
                    distinct: 1164,
                    total: 1164,
                }],
            }),
        }
    }

    /// A report carrying every declared-config section, for the `--schema`
    /// payload: named and unnamed path rules, a creatable rule with required
    /// vars and a body scaffold, an inbox, and a non-empty frontmatter schema.
    fn schema_sample() -> DescribeReport {
        DescribeReport {
            folders: vec!["".into(), "notes".into()],
            path_rules: vec![
                PathRule {
                    glob: "notes/**".into(),
                    name: Some("note".into()),
                    frontmatter_defaults: json!({ "type": "note" }),
                },
                PathRule {
                    glob: "archive/**".into(),
                    name: None,
                    frontmatter_defaults: json!({}),
                },
            ],
            creatable_rules: vec![CreatableRule {
                name: "task".into(),
                target: "tasks/{{var.slug}}.md".into(),
                required_vars: vec!["slug".into()],
                frontmatter_defaults: json!({ "type": "task", "status": "backlog" }),
                body: Some("# {{var.slug}}\n\n".into()),
            }],
            inbox: Some("inbox".into()),
            schema: declared_schema_value(),
            data: None,
        }
    }

    #[test]
    fn summary_text_renders_structure_counts_then_data() {
        let s = summary_text(&describe_sample());
        assert!(s.starts_with("folders    2\n"), "{s}");
        assert!(
            s.contains("1164 documents · created 2026-05-10 → 2026-07-03"),
            "{s}"
        );
        assert!(s.contains("type: note 575 · task 420 (+4 more)"), "{s}");
        assert!(s.contains("(skipped: title 1164/1164)"), "{s}");
    }

    /// Every count line prints, zero or not, and an unconfigured inbox prints
    /// `(none)` rather than dropping its line — a dropped line is indistinguish-
    /// able from a zero count when reading the block.
    #[test]
    fn summary_text_prints_zero_counts_and_an_absent_inbox() {
        let mut report = describe_sample();
        report.data = None;
        assert_eq!(
            summary_text(&report),
            "folders    2\npath rules 0\ncreatable  0\ninbox      (none)\n"
        );
    }

    /// The two slots cannot fork: every records count line names a key of the
    /// json object and carries that key's value. Run over a configured vault
    /// (inbox set, non-zero rule counts) AND a config-less one (null inbox,
    /// zero counts), so the `(none)` spelling of a null is covered too. The
    /// contents-summary is cleared on both: `data` is the one key with no count
    /// line, and the key-count assertion holds only in its absence.
    #[test]
    fn summary_text_lines_mirror_the_summary_json_object() {
        for mut report in [schema_sample(), describe_sample()] {
            report.data = None;
            let text = summary_text(&report);
            let json: serde_json::Value = serde_json::from_str(&summary_json(&report)).unwrap();

            let rendered: Vec<(&str, String)> = text
                .lines()
                .take_while(|line| !line.is_empty())
                .map(|line| {
                    let (label, value) = line.split_at(11);
                    (label.trim_end(), value.trim().to_string())
                })
                .collect();
            assert_eq!(
                rendered
                    .iter()
                    .map(|(label, _)| *label)
                    .collect::<Vec<&str>>(),
                vec!["folders", "path rules", "creatable", "inbox"]
            );

            // Each records label maps to the json key it projects; `(none)` is
            // the records spelling of a `null` inbox.
            let keys = ["folders", "path_rules", "creatable_rules", "inbox"];
            for ((_, value), key) in rendered.iter().zip(keys) {
                let expected = match &json[key] {
                    serde_json::Value::Null => NO_INBOX.to_string(),
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                assert_eq!(*value, expected, "records/json fork on `{key}`");
            }
            assert_eq!(
                json.as_object().unwrap().len(),
                keys.len(),
                "a json key with no records line: {json}"
            );
        }
    }

    /// The default `--format json` is the projection of the default records
    /// block: structure COUNTS plus the same contents-summary, and no declared
    /// config (the folder list, the rules, the schema stay behind `--schema`).
    #[test]
    fn summary_json_projects_the_records_summary() {
        let mut report = schema_sample();
        report.data = describe_sample().data;
        let text = summary_json(&report);
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["folders"], 2);
        assert_eq!(v["path_rules"], 2);
        assert_eq!(v["creatable_rules"], 1);
        assert_eq!(v["inbox"], "inbox");
        assert_eq!(v["data"]["total"], 1164);
        assert_eq!(v["data"]["fields"][0]["values"][0]["value"], "note");
        assert!(v.get("schema").is_none(), "{text}");
        assert!(
            text.starts_with(r#"{"folders":2,"path_rules":2,"creatable_rules":1,"inbox":"inbox""#),
            "{text}"
        );
    }

    #[test]
    fn summary_json_omits_data_when_not_requested() {
        let text = summary_json(&schema_sample());
        assert_eq!(
            text,
            r#"{"folders":2,"path_rules":2,"creatable_rules":1,"inbox":"inbox"}"#
        );
    }

    #[test]
    fn summary_json_carries_a_null_inbox_when_unconfigured() {
        let mut report = schema_sample();
        report.inbox = None;
        let v: serde_json::Value = serde_json::from_str(&summary_json(&report)).unwrap();
        assert!(v["inbox"].is_null());
    }

    #[test]
    fn schema_json_serializes_the_whole_report() {
        let text = schema_json(&schema_sample());
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["folders"][1], "notes");
        assert_eq!(v["path_rules"][0]["glob"], "notes/**");
        assert_eq!(v["path_rules"][0]["frontmatter_defaults"]["type"], "note");
        assert_eq!(v["creatable_rules"][0]["target"], "tasks/{{var.slug}}.md");
        assert_eq!(v["schema"]["rules"][0]["required_frontmatter"][0], "type");
        assert!(text.starts_with(r#"{"folders":"#), "{text}");
    }

    /// `--schema --format json` is the whole report verbatim — no projection,
    /// no re-ordering, no dropped key — over both report shapes.
    #[test]
    fn schema_json_is_the_report_serialized_verbatim() {
        for report in [schema_sample(), describe_sample()] {
            assert_eq!(
                schema_json(&report),
                serde_json::to_string(&report).unwrap()
            );
        }
    }

    /// The `--schema` records block carries the same content `schema_json`
    /// serializes: every folder, every rule with its defaults, inbox, schema.
    #[test]
    fn schema_text_renders_the_declared_config() {
        let s = schema_text(&schema_sample());
        assert!(s.starts_with("folders\n  (root)\n  notes\n"), "{s}");
        assert!(s.contains("path rules\n  notes/** (note)\n"), "{s}");
        assert!(s.contains("    defaults:\n      type: note\n"), "{s}");
        // A rule with no declared defaults prints its glob alone.
        assert!(s.contains("  archive/**\n"), "{s}");
        assert!(
            s.contains("creatable rules\n  task → tasks/{{var.slug}}.md\n"),
            "{s}"
        );
        assert!(s.contains("    required vars: slug\n"), "{s}");
        assert!(s.contains("    body: 2 lines\n"), "{s}");
        assert!(s.contains("inbox\n  inbox\n"), "{s}");
        assert!(s.contains("schema\n  ignore: []\n"), "{s}");
        assert!(s.contains("  required_frontmatter:\n  - type\n"), "{s}");
        assert!(s.contains("  rules:\n"), "{s}");
    }

    #[test]
    fn schema_text_appends_the_data_block_when_requested() {
        let mut report = schema_sample();
        report.data = describe_sample().data;
        let s = schema_text(&report);
        assert!(s.contains("schema\n"), "{s}");
        assert!(s.contains("1164 documents · created"), "{s}");
    }

    /// A config-less vault declares nothing: only the folder listing prints. The
    /// schema section in particular stays out — `describe` reports a default
    /// `ValidateConfig` there, and rendering its three empty keys would read as
    /// a declared schema.
    #[test]
    fn schema_text_on_a_config_less_vault_lists_only_folders() {
        let mut report = describe_sample();
        report.data = None;
        assert_eq!(schema_text(&report), "folders\n  (root)\n  notes\n");
    }

    /// The same omission rule with a config in play: a vault whose config
    /// declares rules but no `validate` section prints its rules and no schema.
    #[test]
    fn schema_text_omits_the_schema_section_of_a_validate_less_config() {
        let mut report = schema_sample();
        report.schema = undeclared_schema();
        let s = schema_text(&report);
        assert!(s.contains("path rules\n"), "{s}");
        assert!(s.contains("creatable rules\n"), "{s}");
        assert!(s.contains("inbox\n  inbox\n"), "{s}");
        assert!(!s.contains("schema"), "{s}");
    }

    fn describe_view() -> DescribeView {
        DescribeView {
            report: describe_sample(),
            by: vec![],
            schema: false,
            no_pager: false,
            format: FormatChoice {
                explicit: Some(Format::Json),
                spec: FormatSpec {
                    tty: Format::Records,
                    piped: Format::Records,
                },
            },
        }
    }

    /// `--schema --format json` is the surface the whole declared config lives
    /// on; the default `--format json` never carries it.
    #[test]
    fn render_describe_routes_json_by_the_schema_flag() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut presenter = Presenter::new(&mut out, &mut err);
            assert_eq!(drive(describe_view(), &mut presenter), EXIT_OK);
        }
        let summary = String::from_utf8(out).unwrap();
        assert!(!summary.contains("\"schema\""), "{summary}");
        assert!(summary.starts_with(r#"{"folders":2,"#), "{summary}");

        let mut view = describe_view();
        view.report = schema_sample();
        view.schema = true;
        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut presenter = Presenter::new(&mut out, &mut err);
            assert_eq!(drive(view, &mut presenter), EXIT_OK);
        }
        let dump = String::from_utf8(out).unwrap();
        assert!(dump.contains(r#""schema":{"ignore":[]"#), "{dump}");
        assert!(dump.contains(r#""rules":[{"#), "{dump}");
        assert!(dump.contains(r#""folders":["","notes"]"#), "{dump}");
    }

    #[test]
    fn render_describe_tolerates_broken_pipe() {
        let mut err = Vec::new();
        let code = {
            let mut presenter = Presenter::new(FailingWriter(io::ErrorKind::BrokenPipe), &mut err);
            drive(describe_view(), &mut presenter)
        };
        assert_eq!(code, EXIT_OK);
        assert!(err.is_empty());
    }

    #[test]
    fn render_describe_reports_other_io_errors() {
        let mut err = Vec::new();
        let code = {
            let mut presenter =
                Presenter::new(FailingWriter(io::ErrorKind::PermissionDenied), &mut err);
            drive(describe_view(), &mut presenter)
        };
        assert_eq!(code, EXIT_OPERATIONAL);
        assert!(String::from_utf8(err).unwrap().starts_with("norn: "));
    }

    #[test]
    fn warn_unknown_by_describe_warns_when_the_field_was_dropped() {
        let report = describe_sample(); // data.fields carries only "type"
        let mut err = Vec::new();
        let mut conv = Conversation::new(&mut err);
        warn_unknown_by_describe(&report, &["priorty".to_string()], &mut conv).unwrap();
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "warning: --by field `priorty` not present in any matching document\n"
        );
    }

    #[test]
    fn warn_unknown_by_describe_silent_when_the_field_is_present() {
        let report = describe_sample();
        let mut err = Vec::new();
        let mut conv = Conversation::new(&mut err);
        warn_unknown_by_describe(&report, &["type".to_string()], &mut conv).unwrap();
        assert!(err.is_empty());
    }

    #[test]
    fn warn_unknown_by_describe_skips_when_data_mode_is_off() {
        let mut report = describe_sample();
        report.data = None;
        let mut err = Vec::new();
        let mut conv = Conversation::new(&mut err);
        warn_unknown_by_describe(&report, &["priorty".to_string()], &mut conv).unwrap();
        assert!(err.is_empty(), "no --data/--by was requested: {err:?}");
    }

    #[test]
    fn warn_unknown_by_describe_skips_a_zero_match_result() {
        let mut report = describe_sample();
        report.data.as_mut().unwrap().total = 0;
        let mut err = Vec::new();
        let mut conv = Conversation::new(&mut err);
        warn_unknown_by_describe(&report, &["priorty".to_string()], &mut conv).unwrap();
        assert!(
            err.is_empty(),
            "a zero-match result must not warn on every field: {err:?}"
        );
    }

    #[test]
    fn warn_unknown_by_describe_ignores_whitespace_only_entries() {
        let report = describe_sample();
        let mut err = Vec::new();
        let mut conv = Conversation::new(&mut err);
        warn_unknown_by_describe(&report, &[" ".to_string(), "".to_string()], &mut conv).unwrap();
        assert!(err.is_empty());
    }

    #[test]
    fn render_describe_with_unknown_by_field_still_exits_ok_and_warns() {
        let mut view = describe_view();
        view.by = vec!["priorty".to_string()];
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut presenter = Presenter::new(&mut out, &mut err);
            drive(view, &mut presenter)
        };
        assert_eq!(code, EXIT_OK);
        assert!(!out.is_empty());
        assert!(String::from_utf8(err)
            .unwrap()
            .contains("--by field `priorty`"));
    }
}
