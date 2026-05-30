//! Renderers for `norn show`.

use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::Write;

use crate::output::palette::Palette;
use crate::output::primitives::{record_block, separator, Field};
use crate::output::projection::{
    filter_frontmatter, frontmatter_to_display, headings_to_display, incoming_links_to_display,
    json_value_inline, outgoing_links_to_display, split_cols, unknown_facet_message,
    unresolved_links_to_display, KNOWN_FACETS,
};

/// Warn to `stderr` for `--col` tokens that won't resolve: a dot-prefixed facet
/// that isn't a known structural facet, or a bare frontmatter field absent from
/// every record. Fires once per token, not per record (mirrors `norn find`).
pub fn warn_unknown_cols(
    cols: &[String],
    report: &super::ShowReport,
    stderr: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    let (facets, fields) = split_cols(cols);
    for facet in &facets {
        if !KNOWN_FACETS.contains(&facet.as_str()) {
            writeln!(stderr, "warn: {}", unknown_facet_message(facet))?;
        }
    }
    for field in &fields {
        let present_in_any = report.records.iter().any(|r| {
            r.frontmatter
                .as_ref()
                .and_then(|fm| fm.as_object())
                .is_some_and(|obj| obj.contains_key(field))
        });
        if !present_in_any {
            writeln!(
                stderr,
                "warn: --col field '{field}' not present in document (bare names select frontmatter fields; use '.{field}' for a structural facet)"
            )?;
        }
    }
    Ok(())
}

/// JSON output. Always emits an array of records. `--col` filters which
/// fields appear; `path` is always present as identity context.
pub fn render_json_with_col(report: &super::ShowReport, cols: &[String]) -> String {
    let array: Vec<Value> = report
        .records
        .iter()
        .map(|r| narrow_to_json(r, cols))
        .collect();
    serde_json::to_string(&array).unwrap()
}

/// Convenience: emit JSON with all default fields.
// Only called from unit-test helpers; suppress dead_code for non-test builds.
#[cfg(test)]
pub fn render_json(report: &super::ShowReport) -> String {
    render_json_with_col(report, &[])
}

fn narrow_to_json(record: &super::ShowRecord, cols: &[String]) -> Value {
    if cols.is_empty() {
        serde_json::to_value(record).unwrap()
    } else {
        let (facets, fields) = split_cols(cols);
        let allow: HashSet<&str> = facets.iter().map(String::as_str).collect();
        let mut obj = json!({ "path": record.path });
        let map = obj.as_object_mut().unwrap();
        // `.frontmatter` emits the whole block; bare field names filter it to
        // just those keys (matching `norn find`'s frontmatter projection).
        if allow.contains("frontmatter") {
            map.insert(
                "frontmatter".into(),
                serde_json::to_value(&record.frontmatter).unwrap(),
            );
        } else if !fields.is_empty() {
            map.insert(
                "frontmatter".into(),
                filter_frontmatter(record.frontmatter.as_ref(), &fields),
            );
        }
        if allow.contains("headings") {
            map.insert(
                "headings".into(),
                serde_json::to_value(&record.headings).unwrap(),
            );
        }
        if allow.contains("outgoing_links") {
            map.insert(
                "outgoing_links".into(),
                serde_json::to_value(&record.outgoing_links).unwrap(),
            );
        }
        if allow.contains("unresolved_links") {
            map.insert(
                "unresolved_links".into(),
                serde_json::to_value(&record.unresolved_links).unwrap(),
            );
        }
        if allow.contains("incoming_links") {
            map.insert(
                "incoming_links".into(),
                serde_json::to_value(&record.incoming_links).unwrap(),
            );
        }
        if allow.contains("body") {
            map.insert("body".into(), serde_json::to_value(&record.body).unwrap());
        }
        // `.raw` last: the heaviest/most-derived facet (whole source file from
        // disk). Omit the key when the file was unreadable.
        if allow.contains("raw") {
            if let Some(raw) = &record.raw {
                map.insert("raw".into(), serde_json::Value::String(raw.clone()));
            }
        }
        obj
    }
}

/// `paths` output: one document path per line. `--col` is inert (the caller
/// warns); identity is all this format carries.
pub fn render_paths(report: &super::ShowReport) -> String {
    let mut buf = String::new();
    for record in &report.records {
        buf.push_str(record.path.as_str());
        buf.push('\n');
    }
    buf
}

/// `jsonl` output: one JSON record object per line, `--col`-narrowed the same
/// way as the `json` array. The line-per-record shape is the streaming sibling
/// of [`render_json_with_col`].
pub fn render_jsonl_with_col(report: &super::ShowReport, cols: &[String]) -> String {
    let mut buf = String::new();
    for record in &report.records {
        buf.push_str(&serde_json::to_string(&narrow_to_json(record, cols)).unwrap());
        buf.push('\n');
    }
    buf
}

/// `records` output with optional `--col` narrowing.
///
/// Emits one records-block per [`ShowRecord`], separated by a horizontal-rule
/// line between records. Default field order: each frontmatter field, then
/// `headings`, `outgoing_links`, `unresolved_links`, `incoming_links`, `body`
/// (only when present). `path` is always emitted as the record-block header.
///
/// When `cols` is non-empty, only fields whose names appear in `cols` are
/// emitted (plus `path` which is always the header).
///
/// Empty fields (no headings, no links, etc.) are omitted silently.
pub fn render_records_with_col(report: &super::ShowReport, cols: &[String]) -> String {
    let palette = Palette::off();
    let term_width = terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80);

    let (facets, field_cols) = split_cols(cols);
    let facet_set: HashSet<&str> = facets.iter().map(String::as_str).collect();
    let all_cols = cols.is_empty();

    let mut buf: Vec<u8> = Vec::new();

    for (i, record) in report.records.iter().enumerate() {
        if i > 0 {
            separator(&mut buf, &palette, term_width).unwrap();
        }

        let owned = build_text_fields(record, all_cols, &facet_set, &field_cols);
        let fields: Vec<Field<'_>> = owned.iter().map(FieldOwned::as_field).collect();
        record_block(
            &mut buf,
            &palette,
            Some(record.path.as_str()),
            &fields,
            term_width,
        )
        .unwrap();

        if fields.is_empty() {
            writeln!(buf, "  (no fields)").unwrap();
        }
    }

    String::from_utf8(buf).unwrap()
}

/// Convenience: emit records with all default fields.
// Only called from unit-test helpers; suppress dead_code for non-test builds.
#[cfg(test)]
pub fn render_records(report: &super::ShowReport) -> String {
    render_records_with_col(report, &[])
}

// ---------------------------------------------------------------------------
// Text-field builders
// ---------------------------------------------------------------------------

/// Build the ordered [`Field`] slice for a single record.
///
/// Field order: frontmatter → headings → outgoing_links → unresolved_links →
/// incoming_links → body.  Empty fields are omitted.  `cols` gate applies
/// when `all_cols` is false.
fn build_text_fields(
    record: &super::ShowRecord,
    all_cols: bool,
    facet_set: &HashSet<&str>,
    field_cols: &[String],
) -> Vec<FieldOwned> {
    let mut fields: Vec<FieldOwned> = Vec::new();

    // Bare --col names project individual frontmatter fields as their own
    // labeled lines (matching `norn find`), in the order requested.
    if !field_cols.is_empty() {
        if let Some(serde_json::Value::Object(obj)) = &record.frontmatter {
            for key in field_cols {
                if let Some(value) = obj.get(key) {
                    fields.push(FieldOwned {
                        label: key.clone(),
                        value: json_value_inline(value),
                    });
                }
            }
        }
    }

    // Default (no `--col`): every frontmatter key as its own labeled line,
    // matching `norn find`'s records projection — a bare field is a column.
    if all_cols {
        if let Some(serde_json::Value::Object(obj)) = &record.frontmatter {
            for (key, value) in obj {
                fields.push(FieldOwned {
                    label: key.clone(),
                    value: json_value_inline(value),
                });
            }
        }
    }

    // `.frontmatter` facet emits the whole consolidated block (recovers the
    // pre-unification default form).
    if facet_set.contains("frontmatter") {
        if let Some(fm) = &record.frontmatter {
            let value = frontmatter_to_display(fm);
            if !value.is_empty() {
                fields.push(FieldOwned {
                    label: "frontmatter".into(),
                    value,
                });
            }
        }
    }

    if (all_cols || facet_set.contains("headings")) && !record.headings.is_empty() {
        let value = headings_to_display(&record.headings);
        fields.push(FieldOwned {
            label: "headings".into(),
            value,
        });
    }

    if (all_cols || facet_set.contains("outgoing_links")) && !record.outgoing_links.is_empty() {
        let value = outgoing_links_to_display(&record.outgoing_links);
        fields.push(FieldOwned {
            label: "outgoing_links".into(),
            value,
        });
    }

    if (all_cols || facet_set.contains("unresolved_links")) && !record.unresolved_links.is_empty() {
        let value = unresolved_links_to_display(&record.unresolved_links);
        fields.push(FieldOwned {
            label: "unresolved_links".into(),
            value,
        });
    }

    if (all_cols || facet_set.contains("incoming_links")) && !record.incoming_links.is_empty() {
        let value = incoming_links_to_display(&record.incoming_links);
        fields.push(FieldOwned {
            label: "incoming_links".into(),
            value,
        });
    }

    if all_cols || facet_set.contains("body") {
        if let Some(body) = &record.body {
            if !body.trim().is_empty() {
                fields.push(FieldOwned {
                    label: "body".into(),
                    value: body.trim().to_string(),
                });
            }
        }
    }

    // `.raw` last, and never in the default dump (heavy/disk; opt-in by name).
    if facet_set.contains("raw") {
        if let Some(raw) = &record.raw {
            if !raw.is_empty() {
                fields.push(FieldOwned {
                    label: "raw".into(),
                    value: raw.clone(),
                });
            }
        }
    }

    fields
}

/// Owned version of [`Field`] for building before borrowing into [`Field`] slices.
struct FieldOwned {
    label: String,
    value: String,
}

// ---------------------------------------------------------------------------
// Helper: convert FieldOwned → Field<'_> for record_block
// ---------------------------------------------------------------------------

impl FieldOwned {
    fn as_field(&self) -> Field<'_> {
        Field {
            label: self.label.as_str(),
            value: self.value.as_str(),
            highlight: false,
        }
    }
}
