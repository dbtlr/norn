//! Format-specific output renderers (paths / records / json / jsonl).

use crate::cache::{DocumentDeep, FindResult};
use std::collections::HashSet;
use std::io::Write;

use crate::cli::{FindArgs, FindFormat};
use crate::output::primitives::{count_line, record_block, separator, Field};
use crate::output::projection::{
    filter_frontmatter, frontmatter_to_display, headings_to_display, incoming_links_to_display,
    json_value_inline, outgoing_links_to_display, split_cols, unresolved_links_to_display,
};

/// Fetch the deep record for the match at `i`, if a deep fetch was performed
/// and it succeeded. Returns `None` both when no deep fetch ran (cheap-facet
/// path) and when the fetch yielded nothing for this doc (treated as empty).
fn deep_at(deep: &[Option<DocumentDeep>], i: usize) -> Option<&DocumentDeep> {
    deep.get(i).and_then(|d| d.as_ref())
}

/// The `.raw` value for the match at `i`, if a disk read was performed and
/// succeeded. Returns `None` both when no read ran (`.raw` not requested) and
/// when the file was unreadable.
fn raw_at(raw: &[Option<String>], i: usize) -> Option<&str> {
    raw.get(i).and_then(|r| r.as_deref())
}

#[allow(clippy::too_many_arguments)]
pub fn render(
    result: &FindResult,
    deep: &[Option<DocumentDeep>],
    raw: &[Option<String>],
    args: &FindArgs,
    format: FindFormat,
    sort_field: Option<&str>,
    sort_direction: Option<&str>,
    starts_at: usize,
    palette: &crate::output::palette::Palette,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> std::io::Result<()> {
    match format {
        FindFormat::Paths => render_paths(result, stdout, stderr),
        FindFormat::Json => render_json(
            result,
            deep,
            raw,
            args,
            sort_field,
            sort_direction,
            starts_at,
            stdout,
        ),
        FindFormat::Jsonl => render_jsonl(result, deep, raw, args, stdout, stderr),
        FindFormat::Records => render_records(result, deep, raw, args, palette, stdout, stderr),
    }
}

fn render_paths(
    result: &FindResult,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> std::io::Result<()> {
    for doc in &result.matches {
        writeln!(stdout, "{}", doc.path)?;
    }
    if result.truncated {
        writeln!(
            stderr,
            "note: showing {} of {} (--no-limit for all)",
            result.returned, result.total
        )?;
    }
    Ok(())
}

/// Build the JSON object for a single matched document under `--col`.
///
/// Mirrors `get`'s `narrow_to_json` shape and key ordering. With no `--col`,
/// the legacy find shape is preserved: `{path, frontmatter}` with the whole
/// frontmatter block. With `--col`, only the requested facets / fields appear
/// (plus `path` as identity). Cheap facets (`.frontmatter`, `.body`) read from
/// the `DocumentSummary`; join-backed facets read from `deep`.
pub(crate) fn doc_to_json(
    doc: &crate::core::DocumentSummary,
    deep: Option<&DocumentDeep>,
    raw: Option<&str>,
    cols: &[String],
    all_cols: bool,
) -> serde_json::Value {
    if cols.is_empty() && !all_cols {
        return serde_json::json!({
            "path": doc.path.as_str(),
            "frontmatter": filter_frontmatter(doc.frontmatter.as_ref(), &[]),
        });
    }

    let (facets, fields) = split_cols(cols);
    let allow: HashSet<&str> = facets.iter().map(String::as_str).collect();
    let mut obj = serde_json::json!({ "path": doc.path.as_str() });
    let map = obj.as_object_mut().unwrap();

    // `.stem` is identity-class (like `.path`): opt-in only, never in the
    // default or `--all-cols` dump. Placed right after `path`.
    if allow.contains("stem") {
        map.insert("stem".into(), serde_json::Value::String(doc.stem.clone()));
    }

    // `.document_hash` is identity/metadata-class: opt-in only (never in the
    // default or `--all-cols` dump), so existing output stays byte-identical.
    // The full-content blake3 the CAS surfaces use — how a caller reads the hash
    // to feed `edit --expected-hash`. Omit for an unreadable/unindexed file
    // (empty hash) rather than hand out "" as a bogus CAS token.
    if allow.contains("document_hash") && !doc.hash.is_empty() {
        map.insert(
            "document_hash".into(),
            serde_json::Value::String(doc.hash.clone()),
        );
    }

    // `--all-cols`: whole frontmatter + every cache-served facet + body.
    // `.frontmatter` emits the whole block; bare field names filter it.
    if all_cols || allow.contains("frontmatter") {
        map.insert(
            "frontmatter".into(),
            filter_frontmatter(doc.frontmatter.as_ref(), &[]),
        );
    } else if !fields.is_empty() {
        map.insert(
            "frontmatter".into(),
            filter_frontmatter(doc.frontmatter.as_ref(), &fields),
        );
    }
    if all_cols || allow.contains("headings") {
        let headings = deep.map(|d| d.headings.as_slice()).unwrap_or(&[]);
        map.insert("headings".into(), serde_json::to_value(headings).unwrap());
    }
    if all_cols || allow.contains("outgoing_links") {
        let links = deep.map(|d| d.outgoing_links.as_slice()).unwrap_or(&[]);
        map.insert(
            "outgoing_links".into(),
            serde_json::to_value(links).unwrap(),
        );
    }
    if all_cols || allow.contains("unresolved_links") {
        let links = deep.map(|d| d.unresolved_links.as_slice()).unwrap_or(&[]);
        map.insert(
            "unresolved_links".into(),
            serde_json::to_value(links).unwrap(),
        );
    }
    if all_cols || allow.contains("incoming_links") {
        let links = deep.map(|d| d.incoming_links.as_slice()).unwrap_or(&[]);
        map.insert(
            "incoming_links".into(),
            serde_json::to_value(links).unwrap(),
        );
    }
    if all_cols || allow.contains("body") {
        map.insert(
            "body".into(),
            serde_json::Value::String(doc.body_text.clone()),
        );
    }
    // `.raw` last: byte-faithful whole source file from disk. Omit when
    // unreadable. Never emitted by `--all-cols` (cache-only dump).
    if !all_cols && allow.contains("raw") {
        if let Some(raw) = raw {
            map.insert("raw".into(), serde_json::Value::String(raw.to_string()));
        }
    }
    obj
}

#[allow(clippy::too_many_arguments)]
fn render_json(
    result: &FindResult,
    deep: &[Option<DocumentDeep>],
    raw: &[Option<String>],
    args: &FindArgs,
    _sort_field: Option<&str>,
    _sort_direction: Option<&str>,
    starts_at: usize,
    stdout: &mut dyn Write,
) -> std::io::Result<()> {
    let documents: Vec<serde_json::Value> = result
        .matches
        .iter()
        .enumerate()
        .map(|(i, d)| {
            doc_to_json(
                d,
                deep_at(deep, i),
                raw_at(raw, i),
                &args.col,
                args.all_cols,
            )
        })
        .collect();

    let payload = serde_json::json!({
        "total": result.total,
        "returned": result.returned,
        "starts_at": starts_at,
        "documents": documents,
    });
    writeln!(stdout, "{}", serde_json::to_string_pretty(&payload)?)
}

fn render_jsonl(
    result: &FindResult,
    deep: &[Option<DocumentDeep>],
    raw: &[Option<String>],
    args: &FindArgs,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> std::io::Result<()> {
    for (i, doc) in result.matches.iter().enumerate() {
        let line = doc_to_json(
            doc,
            deep_at(deep, i),
            raw_at(raw, i),
            &args.col,
            args.all_cols,
        );
        writeln!(stdout, "{}", serde_json::to_string(&line)?)?;
    }
    if result.truncated {
        writeln!(
            stderr,
            "note: showing {} of {} (--no-limit for all)",
            result.returned, result.total
        )?;
    }
    Ok(())
}

fn render_records(
    result: &FindResult,
    deep: &[Option<DocumentDeep>],
    raw: &[Option<String>],
    args: &FindArgs,
    palette: &crate::output::palette::Palette,
    stdout: &mut dyn Write,
    _stderr: &mut dyn Write,
) -> std::io::Result<()> {
    let term_width = terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80);

    count_line(
        stdout,
        palette,
        result.total,
        result.returned,
        args.paging.starts_at,
        "documents",
    )?;

    if !result.matches.is_empty() {
        writeln!(stdout)?;
    }

    let sort_field = args.paging.sort.as_deref();

    for (i, doc) in result.matches.iter().enumerate() {
        if i > 0 {
            separator(stdout, palette, term_width)?;
        }
        let pairs = build_record_pairs(
            doc,
            deep_at(deep, i),
            raw_at(raw, i),
            &args.col,
            args.all_cols,
        );
        let fields: Vec<Field<'_>> = pairs
            .iter()
            .map(|(k, v)| Field {
                label: k.as_str(),
                value: v.as_str(),
                highlight: sort_field.is_some_and(|sf| sf == k),
            })
            .collect();
        record_block(
            stdout,
            palette,
            Some(doc.path.as_str()),
            &fields,
            term_width,
        )?;
        if pairs.is_empty() {
            let placeholder = if args.col.is_empty() {
                "(no frontmatter)"
            } else {
                "(no matching fields)"
            };
            writeln!(
                stdout,
                "  {}{placeholder}{}",
                palette.dim.render(),
                palette.dim.render_reset()
            )?;
        }
    }
    Ok(())
}

/// Build the ordered `(label, value)` record rows for a single matched doc.
///
/// No `--col`: every frontmatter key as its own labeled row (legacy behavior).
/// With `--col`: bare fields project individual frontmatter keys (in requested
/// order), then the structural facets in `get`'s canonical order — frontmatter,
/// headings, outgoing_links, unresolved_links, incoming_links, body. Cheap
/// facets read from the `DocumentSummary`; join-backed facets read from `deep`
/// (treated as empty when the fetch yielded nothing). Empty facets are omitted.
fn build_record_pairs(
    doc: &crate::core::DocumentSummary,
    deep: Option<&DocumentDeep>,
    raw: Option<&str>,
    cols: &[String],
    all_cols: bool,
) -> Vec<(String, String)> {
    let fm_object = doc.frontmatter.as_ref().and_then(|fm| fm.as_object());

    if cols.is_empty() && !all_cols {
        // Legacy default: every frontmatter key as its own labeled row.
        let mut pairs = Vec::new();
        if let Some(obj) = fm_object {
            for (key, value) in obj {
                pairs.push((key.clone(), json_value_inline(value)));
            }
        }
        return pairs;
    }

    let (facets, fields) = split_cols(cols);
    let facet_set: HashSet<&str> = facets.iter().map(String::as_str).collect();
    let mut pairs = Vec::new();

    // `.stem` is identity-class (like `.path`): opt-in only, never in the
    // default or `--all-cols` dump. Emitted first, adjacent to the path header.
    if facet_set.contains("stem") {
        pairs.push(("stem".into(), doc.stem.clone()));
    }

    // `.document_hash` is identity/metadata-class: opt-in only (never in the
    // default or `--all-cols` dump), so existing output stays byte-identical.
    // The value is the full-content blake3 the CAS surfaces use — this is how a
    // caller reads the hash to feed `edit --expected-hash`. Omit for an
    // unreadable/unindexed file (empty hash) rather than emit a bogus "".
    if facet_set.contains("document_hash") && !doc.hash.is_empty() {
        pairs.push(("document_hash".into(), doc.hash.clone()));
    }

    // `--all-cols`: per-field frontmatter rows (like the default), then every
    // cache-served facet. No consolidated `.frontmatter` block, no `.raw`.
    if all_cols {
        if let Some(obj) = fm_object {
            for (key, value) in obj {
                pairs.push((key.clone(), json_value_inline(value)));
            }
        }
    }

    // Bare fields: individual frontmatter keys, in requested order.
    for field in &fields {
        if let Some(value) = fm_object.and_then(|obj| obj.get(field)) {
            pairs.push((field.clone(), json_value_inline(value)));
        }
    }

    // `.frontmatter`: the whole consolidated block.
    if facet_set.contains("frontmatter") {
        if let Some(fm) = &doc.frontmatter {
            let value = frontmatter_to_display(fm);
            if !value.is_empty() {
                pairs.push(("frontmatter".into(), value));
            }
        }
    }

    if all_cols || facet_set.contains("headings") {
        let headings = deep.map(|d| d.headings.as_slice()).unwrap_or(&[]);
        if !headings.is_empty() {
            pairs.push(("headings".into(), headings_to_display(headings)));
        }
    }

    if all_cols || facet_set.contains("outgoing_links") {
        let links = deep.map(|d| d.outgoing_links.as_slice()).unwrap_or(&[]);
        if !links.is_empty() {
            pairs.push(("outgoing_links".into(), outgoing_links_to_display(links)));
        }
    }

    if all_cols || facet_set.contains("unresolved_links") {
        let links = deep.map(|d| d.unresolved_links.as_slice()).unwrap_or(&[]);
        if !links.is_empty() {
            pairs.push((
                "unresolved_links".into(),
                unresolved_links_to_display(links),
            ));
        }
    }

    if all_cols || facet_set.contains("incoming_links") {
        let links = deep.map(|d| d.incoming_links.as_slice()).unwrap_or(&[]);
        if !links.is_empty() {
            pairs.push(("incoming_links".into(), incoming_links_to_display(links)));
        }
    }

    if all_cols || facet_set.contains("body") {
        let body = doc.body_text.trim();
        if !body.is_empty() {
            pairs.push(("body".into(), body.to_string()));
        }
    }

    // `.raw` last: byte-faithful whole source file from disk. Omit when
    // unreadable or empty. Never emitted by `--all-cols` (cache-only dump).
    if !all_cols && facet_set.contains("raw") {
        if let Some(raw) = raw {
            if !raw.is_empty() {
                pairs.push(("raw".into(), raw.to_string()));
            }
        }
    }

    pairs
}

/// Warn for `--col` tokens that won't resolve: a dot-prefixed facet that isn't
/// a known structural facet, or a bare frontmatter field absent from every
/// matching document. Fires once per token (mirrors `norn get`).
pub fn warn_unknown_cols(
    result: &FindResult,
    cols: &[String],
    stderr: &mut dyn Write,
) -> std::io::Result<()> {
    let (facets, fields) = split_cols(cols);
    for facet in &facets {
        if !crate::output::projection::KNOWN_FACETS.contains(&facet.as_str()) {
            writeln!(
                stderr,
                "warning: {}",
                crate::output::projection::unknown_facet_message(facet)
            )?;
        }
    }
    for field in &fields {
        let present_in_any = result.matches.iter().any(|d| {
            d.frontmatter
                .as_ref()
                .and_then(|fm| fm.as_object())
                .is_some_and(|obj| obj.contains_key(field))
        });
        if !present_in_any {
            writeln!(
                stderr,
                "warning: --col field `{field}` not present in any matching document"
            )?;
        }
    }
    Ok(())
}

pub fn warn_col_ignored_on_paths(
    cols: &[String],
    format: crate::cli::FindFormat,
    stderr: &mut dyn Write,
) -> std::io::Result<()> {
    crate::output::projection::warn_col_ignored(
        cols,
        (format == crate::cli::FindFormat::Paths).then_some("paths"),
        stderr,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::DocumentSummary;
    use camino::Utf8PathBuf;

    fn sample_result() -> FindResult {
        FindResult {
            matches: vec![
                DocumentSummary {
                    path: Utf8PathBuf::from("a.md"),
                    stem: "a".to_string(),
                    hash: "h1".to_string(),
                    frontmatter: Some(serde_json::json!({"type": "note"})),
                    body_text: String::new(),
                },
                DocumentSummary {
                    path: Utf8PathBuf::from("b.md"),
                    stem: "b".to_string(),
                    hash: "h2".to_string(),
                    frontmatter: None,
                    body_text: String::new(),
                },
            ],
            total: 2,
            returned: 2,
            truncated: false,
        }
    }

    #[test]
    fn paths_format_emits_one_path_per_line() {
        let result = sample_result();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        render_paths(&result, &mut stdout, &mut stderr).unwrap();
        assert_eq!(std::str::from_utf8(&stdout).unwrap(), "a.md\nb.md\n");
        assert_eq!(std::str::from_utf8(&stderr).unwrap(), "");
    }

    #[test]
    fn paths_truncated_writes_stderr_signal() {
        let mut result = sample_result();
        result.total = 5;
        result.truncated = true;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        render_paths(&result, &mut stdout, &mut stderr).unwrap();
        assert_eq!(std::str::from_utf8(&stdout).unwrap(), "a.md\nb.md\n");
        let s = std::str::from_utf8(&stderr).unwrap();
        assert!(s.starts_with("note: showing "), "got: {s:?}");
        assert!(s.contains("2 of 5"), "got: {s:?}");
    }

    fn sample_args() -> FindArgs {
        FindArgs {
            filters: crate::filter_args::FilterArgs::default(),
            paging: crate::cli::SortPaginateArgs {
                sort: None,
                desc: false,
                limit: None,
                no_limit: false,
                starts_at: 1,
            },
            format: None,
            all_cols: false,
            col: vec![],
            no_pager: false,
            all: false,
        }
    }

    #[test]
    fn json_format_uses_documents_wrapper_key() {
        let result = sample_result();
        let mut stdout = Vec::new();
        let args = sample_args();
        render_json(&result, &[], &[], &args, None, None, 1, &mut stdout).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(parsed["total"], 2);
        assert_eq!(parsed["returned"], 2);
        assert_eq!(parsed["starts_at"], 1);
        assert!(parsed.get("documents").is_some(), "expected documents key");
        assert!(
            parsed.get("matches").is_none(),
            "matches key should be gone"
        );
        assert_eq!(parsed["documents"][0]["path"], "a.md");
    }

    #[test]
    fn json_omits_truncated_and_sort_keys() {
        let mut result = sample_result();
        result.total = 5;
        result.truncated = true;
        let mut stdout = Vec::new();
        let args = sample_args();
        render_json(
            &result,
            &[],
            &[],
            &args,
            Some("modified"),
            Some("desc"),
            1,
            &mut stdout,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert!(
            parsed.get("truncated").is_none(),
            "truncated should be derivable"
        );
        assert!(
            parsed.get("sort").is_none(),
            "sort echoes request and is dropped"
        );
    }

    #[test]
    fn json_col_narrows_frontmatter() {
        let result = sample_result();
        let mut stdout = Vec::new();
        let mut args = sample_args();
        args.col = vec!["type".to_string()];
        render_json(&result, &[], &[], &args, None, None, 1, &mut stdout).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(parsed["documents"][0]["frontmatter"]["type"], "note");
    }

    #[test]
    fn jsonl_format_emits_one_object_per_line() {
        let result = sample_result();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let args = sample_args();
        render_jsonl(&result, &[], &[], &args, &mut stdout, &mut stderr).unwrap();
        let text = std::str::from_utf8(&stdout).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["path"], "a.md");
    }

    #[test]
    fn jsonl_truncated_writes_stderr_signal() {
        let mut result = sample_result();
        result.total = 5;
        result.truncated = true;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let args = sample_args();
        render_jsonl(&result, &[], &[], &args, &mut stdout, &mut stderr).unwrap();
        let s = std::str::from_utf8(&stderr).unwrap();
        assert!(s.starts_with("note: showing "));
        assert!(s.contains("2 of 5"));
    }

    #[test]
    fn records_format_leads_with_count_line() {
        let result = sample_result();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let args = sample_args();
        let palette = crate::output::palette::Palette::off();
        render_records(&result, &[], &[], &args, &palette, &mut stdout, &mut stderr).unwrap();
        let text = std::str::from_utf8(&stdout).unwrap();
        let first_line = text.lines().next().unwrap();
        assert!(
            first_line.starts_with("2 documents"),
            "first line: {first_line:?}"
        );
    }

    #[test]
    fn records_truncated_count_line_shows_window() {
        let mut result = sample_result();
        result.total = 5;
        result.truncated = true;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let args = sample_args();
        let palette = crate::output::palette::Palette::off();
        render_records(&result, &[], &[], &args, &palette, &mut stdout, &mut stderr).unwrap();
        let text = std::str::from_utf8(&stdout).unwrap();
        let first_line = text.lines().next().unwrap();
        assert!(
            first_line.contains("5 documents") && first_line.contains("showing 1–2"),
            "first line: {first_line:?}"
        );
        // No trailing footer.
        assert!(
            !text.contains("(--no-limit"),
            "expected no footer: {text:?}"
        );
    }

    #[test]
    fn records_field_rows_are_two_space_indented() {
        let result = sample_result();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let args = sample_args();
        let palette = crate::output::palette::Palette::off();
        render_records(&result, &[], &[], &args, &palette, &mut stdout, &mut stderr).unwrap();
        let text = std::str::from_utf8(&stdout).unwrap();
        // Field rows look like "  type    note" — starts with 2-space indent.
        assert!(
            text.contains("\n  type"),
            "expected 2-indent field rows: {text:?}"
        );
    }

    #[test]
    fn records_separator_has_no_surrounding_blank_lines() {
        let result = sample_result();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let args = sample_args();
        let palette = crate::output::palette::Palette::off();
        render_records(&result, &[], &[], &args, &palette, &mut stdout, &mut stderr).unwrap();
        let text = std::str::from_utf8(&stdout).unwrap();
        // No blank-line padding around the horizontal rule.
        assert!(!text.contains("\n\n─"), "blank before separator: {text:?}");
        assert!(!text.contains("─\n\n"), "blank after separator: {text:?}");
    }

    #[test]
    fn records_empty_frontmatter_shows_placeholder() {
        let result = sample_result();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let args = sample_args();
        let palette = crate::output::palette::Palette::off();
        render_records(&result, &[], &[], &args, &palette, &mut stdout, &mut stderr).unwrap();
        let text = std::str::from_utf8(&stdout).unwrap();
        // b.md has no frontmatter; it should show the placeholder line.
        assert!(
            text.contains("b.md\n  (no frontmatter)\n"),
            "expected placeholder under empty-frontmatter record: {text:?}"
        );
    }

    #[test]
    fn records_col_with_no_matches_shows_no_matching_fields_placeholder() {
        let result = sample_result();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut args = sample_args();
        // a.md has type=note but no `nonexistent` field.
        args.col = vec!["nonexistent".to_string()];
        let palette = crate::output::palette::Palette::off();
        render_records(&result, &[], &[], &args, &palette, &mut stdout, &mut stderr).unwrap();
        let text = std::str::from_utf8(&stdout).unwrap();
        assert!(
            text.contains("a.md\n  (no matching fields)\n"),
            "expected (no matching fields) under col-filtered record: {text:?}"
        );
        // b.md (no frontmatter at all) also gets the col-aware placeholder
        // when --col is in effect — the user asked for a field, it isn't there.
        assert!(
            text.contains("b.md\n  (no matching fields)\n"),
            "expected (no matching fields) even when frontmatter absent: {text:?}"
        );
        // The unfiltered "(no frontmatter)" message should NOT appear when --col is set.
        assert!(
            !text.contains("(no frontmatter)"),
            "should not say (no frontmatter) when --col is active: {text:?}"
        );
    }

    #[test]
    fn records_empty_frontmatter_placeholder_uses_dim_when_palette_on() {
        let result = sample_result();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let args = sample_args();
        let palette = crate::output::palette::Palette::on();
        render_records(&result, &[], &[], &args, &palette, &mut stdout, &mut stderr).unwrap();
        let text = std::str::from_utf8(&stdout).unwrap();
        // Dim renders as ANSI 256 #244.
        assert!(
            text.contains("\x1b[38;5;244m(no frontmatter)\x1b[0m"),
            "expected dim-wrapped placeholder: {text:?}"
        );
    }

    #[test]
    fn records_path_is_header_not_field() {
        let result = sample_result();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let args = sample_args();
        let palette = crate::output::palette::Palette::off();
        render_records(&result, &[], &[], &args, &palette, &mut stdout, &mut stderr).unwrap();
        let text = std::str::from_utf8(&stdout).unwrap();
        // The path "a.md" appears as a header line at column 0, not as a "  path  a.md" field row.
        // lines[0] = count line, lines[1] = blank, lines[2] = first record header.
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[2], "a.md",
            "expected path as header at lines[2]: {lines:?}"
        );
        // No "path" field label inside record body.
        assert!(
            !text.contains("\n  path"),
            "path should be header, not field: {text:?}"
        );
    }

    #[test]
    fn col_absent_in_all_matches_warns_with_severity_prefix() {
        let result = sample_result();
        let mut stderr = Vec::new();
        let cols = vec!["nonexistent_field".to_string()];
        warn_unknown_cols(&result, &cols, &mut stderr).unwrap();
        let s = std::str::from_utf8(&stderr).unwrap();
        assert!(s.starts_with("warning: --col field "), "got: {s:?}");
        assert!(s.contains("`nonexistent_field`"), "got: {s:?}");
    }

    #[test]
    fn col_with_paths_format_warns_with_severity_prefix() {
        let mut stderr = Vec::new();
        let cols = vec!["title".to_string()];
        warn_col_ignored_on_paths(&cols, crate::cli::FindFormat::Paths, &mut stderr).unwrap();
        let s = std::str::from_utf8(&stderr).unwrap();
        assert_eq!(s, "warning: --col is ignored with --format paths\n");
    }

    #[test]
    fn col_with_non_paths_format_silent() {
        let mut stderr = Vec::new();
        let cols = vec!["title".to_string()];
        warn_col_ignored_on_paths(&cols, crate::cli::FindFormat::Records, &mut stderr).unwrap();
        assert_eq!(stderr.len(), 0);
    }

    #[test]
    fn records_with_no_color_palette_has_no_ansi() {
        let result = sample_result();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let args = sample_args();
        let palette = crate::output::palette::Palette::off();
        render_records(&result, &[], &[], &args, &palette, &mut stdout, &mut stderr).unwrap();
        let text = std::str::from_utf8(&stdout).unwrap();
        assert!(
            !text.contains("\x1b["),
            "expected no ANSI escapes, got: {}",
            text
        );
    }

    #[test]
    fn records_with_ansi_palette_contains_escapes() {
        let result = sample_result();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let args = sample_args();
        let palette = crate::output::palette::Palette::on();
        render_records(&result, &[], &[], &args, &palette, &mut stdout, &mut stderr).unwrap();
        let text = std::str::from_utf8(&stdout).unwrap();
        assert!(
            text.contains("\x1b["),
            "expected ANSI escapes, got: {}",
            text
        );
    }

    // ---- facet rendering (cheap facets need no deep fetch) ----

    fn result_with_body() -> FindResult {
        FindResult {
            matches: vec![DocumentSummary {
                path: Utf8PathBuf::from("a.md"),
                stem: "a".to_string(),
                hash: "h1".to_string(),
                frontmatter: Some(serde_json::json!({"type": "note", "title": "Alpha"})),
                body_text: "  alpha body  ".to_string(),
            }],
            total: 1,
            returned: 1,
            truncated: false,
        }
    }

    #[test]
    fn json_body_facet_reads_from_summary_no_deep() {
        let result = result_with_body();
        let mut stdout = Vec::new();
        let mut args = sample_args();
        args.col = vec![".body".to_string()];
        // No deep slice supplied — `.body` must still resolve from body_text.
        render_json(&result, &[], &[], &args, None, None, 1, &mut stdout).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        let doc = &v["documents"][0];
        assert_eq!(doc["body"], "  alpha body  ");
        assert!(doc.get("frontmatter").is_none());
    }

    #[test]
    fn json_stem_facet_opt_in_and_omitted_by_default() {
        let result = result_with_body();

        // Default (no --col): identity `path` only, no `stem` key.
        let mut out_default = Vec::new();
        let args_default = sample_args();
        render_json(
            &result,
            &[],
            &[],
            &args_default,
            None,
            None,
            1,
            &mut out_default,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out_default).unwrap();
        assert!(
            v["documents"][0].get("stem").is_none(),
            "stem must be opt-in — absent from the default dump"
        );

        // `--col .stem`: stem present, frontmatter absent.
        let mut out_stem = Vec::new();
        let mut args = sample_args();
        args.col = vec![".stem".to_string()];
        render_json(&result, &[], &[], &args, None, None, 1, &mut out_stem).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out_stem).unwrap();
        let doc = &v["documents"][0];
        assert_eq!(doc["stem"], "a");
        assert!(doc.get("frontmatter").is_none());
    }

    #[test]
    fn json_frontmatter_facet_emits_whole_block() {
        let result = result_with_body();
        let mut stdout = Vec::new();
        let mut args = sample_args();
        args.col = vec![".frontmatter".to_string()];
        render_json(&result, &[], &[], &args, None, None, 1, &mut stdout).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        let doc = &v["documents"][0];
        assert_eq!(doc["frontmatter"]["type"], "note");
        assert_eq!(doc["frontmatter"]["title"], "Alpha");
    }

    #[test]
    fn records_body_facet_trims_and_labels() {
        let result = result_with_body();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut args = sample_args();
        args.col = vec![".body".to_string()];
        let palette = crate::output::palette::Palette::off();
        render_records(&result, &[], &[], &args, &palette, &mut stdout, &mut stderr).unwrap();
        let text = std::str::from_utf8(&stdout).unwrap();
        // Trimmed body under a `body` label, no surrounding whitespace.
        assert!(
            text.contains("\n  body"),
            "expected body field row: {text:?}"
        );
        assert!(
            text.contains("alpha body"),
            "expected body content: {text:?}"
        );
    }

    #[test]
    fn records_mixed_bare_field_and_frontmatter_facet() {
        let result = result_with_body();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut args = sample_args();
        args.col = vec!["title".to_string(), ".frontmatter".to_string()];
        let palette = crate::output::palette::Palette::off();
        render_records(&result, &[], &[], &args, &palette, &mut stdout, &mut stderr).unwrap();
        let text = std::str::from_utf8(&stdout).unwrap();
        // Bare `title` row first, then the consolidated `frontmatter` block.
        assert!(text.contains("\n  title"), "expected title row: {text:?}");
        assert!(
            text.contains("\n  frontmatter"),
            "expected frontmatter block: {text:?}"
        );
    }

    #[test]
    fn unknown_facet_warns_with_find_prefix() {
        let result = sample_result();
        let mut stderr = Vec::new();
        let cols = vec![".bogus".to_string()];
        warn_unknown_cols(&result, &cols, &mut stderr).unwrap();
        let s = std::str::from_utf8(&stderr).unwrap();
        assert!(
            s.starts_with("warning: unknown --col facet '.bogus'"),
            "got: {s:?}"
        );
        assert!(
            s.contains("bare names select frontmatter fields"),
            "got: {s:?}"
        );
    }

    #[test]
    fn known_facet_does_not_warn() {
        let result = sample_result();
        let mut stderr = Vec::new();
        let cols = vec![".headings".to_string()];
        warn_unknown_cols(&result, &cols, &mut stderr).unwrap();
        assert_eq!(stderr.len(), 0, "known facet should not warn");
    }
}
