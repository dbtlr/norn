//! Format-specific output renderers (paths / records / json / jsonl).

use crate::cache::FindResult;
use std::io::Write;

use crate::cli::{FindArgs, FindFormat};
use crate::output::primitives::{count_line, record_block, separator, Field};

#[allow(clippy::too_many_arguments)]
pub fn render(
    result: &FindResult,
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
        FindFormat::Json => {
            render_json(result, args, sort_field, sort_direction, starts_at, stdout)
        }
        FindFormat::Jsonl => render_jsonl(result, args, stdout, stderr),
        FindFormat::Records => render_records(result, args, palette, stdout, stderr),
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

fn render_json(
    result: &FindResult,
    args: &FindArgs,
    _sort_field: Option<&str>,
    _sort_direction: Option<&str>,
    starts_at: usize,
    stdout: &mut dyn Write,
) -> std::io::Result<()> {
    let documents: Vec<serde_json::Value> = result
        .matches
        .iter()
        .map(|d| {
            let frontmatter = filter_frontmatter(d.frontmatter.as_ref(), &args.col);
            serde_json::json!({
                "path": d.path.as_str(),
                "frontmatter": frontmatter,
            })
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

/// Apply --col filtering to a frontmatter object. Empty `cols` = no filter.
fn filter_frontmatter(fm: Option<&serde_json::Value>, cols: &[String]) -> serde_json::Value {
    if cols.is_empty() {
        return fm.cloned().unwrap_or(serde_json::Value::Null);
    }
    let Some(serde_json::Value::Object(obj)) = fm else {
        return serde_json::Value::Object(serde_json::Map::new());
    };
    let mut filtered = serde_json::Map::new();
    for col in cols {
        if let Some(v) = obj.get(col) {
            filtered.insert(col.clone(), v.clone());
        }
    }
    serde_json::Value::Object(filtered)
}

fn render_jsonl(
    result: &FindResult,
    args: &FindArgs,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> std::io::Result<()> {
    for doc in &result.matches {
        let frontmatter = filter_frontmatter(doc.frontmatter.as_ref(), &args.col);
        let line = serde_json::json!({
            "path": doc.path.as_str(),
            "frontmatter": frontmatter,
        });
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
        args.starts_at,
        "documents",
    )?;

    if !result.matches.is_empty() {
        writeln!(stdout)?;
    }

    let sort_field = args.sort.as_deref();

    for (i, doc) in result.matches.iter().enumerate() {
        if i > 0 {
            separator(stdout, palette, term_width)?;
        }
        let pairs = build_record_pairs(doc, &args.col);
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

fn build_record_pairs(doc: &vault_core::DocumentSummary, cols: &[String]) -> Vec<(String, String)> {
    let fm_object = doc.frontmatter.as_ref().and_then(|fm| fm.as_object());
    let field_iter: Vec<String> = if cols.is_empty() {
        fm_object
            .map(|obj| obj.keys().cloned().collect())
            .unwrap_or_default()
    } else {
        cols.to_vec()
    };
    let mut pairs = Vec::new();
    for field in &field_iter {
        if let Some(value) = fm_object.and_then(|obj| obj.get(field)) {
            pairs.push((field.clone(), json_value_to_display_string(value)));
        }
    }
    pairs
}

fn json_value_to_display_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .map(json_value_to_display_string)
            .collect::<Vec<_>>()
            .join(", "),
        serde_json::Value::Object(_) => value.to_string(),
    }
}

pub fn warn_absent_cols(
    result: &FindResult,
    cols: &[String],
    stderr: &mut dyn Write,
) -> std::io::Result<()> {
    for col in cols {
        let present_in_any = result.matches.iter().any(|d| {
            d.frontmatter
                .as_ref()
                .and_then(|fm| fm.as_object())
                .is_some_and(|obj| obj.contains_key(col))
        });
        if !present_in_any {
            writeln!(
                stderr,
                "warning: --col field `{col}` not present in any matching document"
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
    if !cols.is_empty() && format == crate::cli::FindFormat::Paths {
        writeln!(stderr, "warning: --col is ignored with --format paths")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use vault_core::DocumentSummary;

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
            sort: None,
            desc: false,
            limit: 10,
            no_limit: false,
            starts_at: 1,
            format: None,
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
        render_json(&result, &args, None, None, 1, &mut stdout).unwrap();
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
        render_json(&result, &args, None, None, 1, &mut stdout).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(parsed["documents"][0]["frontmatter"]["type"], "note");
    }

    #[test]
    fn jsonl_format_emits_one_object_per_line() {
        let result = sample_result();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let args = sample_args();
        render_jsonl(&result, &args, &mut stdout, &mut stderr).unwrap();
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
        render_jsonl(&result, &args, &mut stdout, &mut stderr).unwrap();
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
        render_records(&result, &args, &palette, &mut stdout, &mut stderr).unwrap();
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
        render_records(&result, &args, &palette, &mut stdout, &mut stderr).unwrap();
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
        render_records(&result, &args, &palette, &mut stdout, &mut stderr).unwrap();
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
        render_records(&result, &args, &palette, &mut stdout, &mut stderr).unwrap();
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
        render_records(&result, &args, &palette, &mut stdout, &mut stderr).unwrap();
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
        render_records(&result, &args, &palette, &mut stdout, &mut stderr).unwrap();
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
        render_records(&result, &args, &palette, &mut stdout, &mut stderr).unwrap();
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
        render_records(&result, &args, &palette, &mut stdout, &mut stderr).unwrap();
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
        warn_absent_cols(&result, &cols, &mut stderr).unwrap();
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
        render_records(&result, &args, &palette, &mut stdout, &mut stderr).unwrap();
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
        render_records(&result, &args, &palette, &mut stdout, &mut stderr).unwrap();
        let text = std::str::from_utf8(&stdout).unwrap();
        assert!(
            text.contains("\x1b["),
            "expected ANSI escapes, got: {}",
            text
        );
    }
}
