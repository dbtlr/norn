//! Format-specific output renderers (paths / records / json / jsonl).

use std::io::Write;
use vault_cache::FindResult;

use crate::cli::{FindArgs, FindFormat};

#[allow(clippy::too_many_arguments)]
pub fn render(
    result: &FindResult,
    args: &FindArgs,
    format: FindFormat,
    sort_field: Option<&str>,
    sort_direction: Option<&str>,
    starts_at: usize,
    palette: &crate::find::color::Palette,
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
            "vault find: showing {} of {} (--no-limit for all)",
            result.returned, result.total
        )?;
    }
    Ok(())
}

fn render_json(
    result: &FindResult,
    args: &FindArgs,
    sort_field: Option<&str>,
    sort_direction: Option<&str>,
    starts_at: usize,
    stdout: &mut dyn Write,
) -> std::io::Result<()> {
    let matches: Vec<serde_json::Value> = result
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

    let sort = match (sort_field, sort_direction) {
        (Some(f), Some(d)) => Some(serde_json::json!({ "field": f, "direction": d })),
        _ => None,
    };

    let payload = serde_json::json!({
        "matches": matches,
        "total": result.total,
        "returned": result.returned,
        "truncated": result.truncated,
        "sort": sort,
        "starts_at": starts_at,
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
            "vault find: showing {} of {} (--no-limit for all)",
            result.returned, result.total
        )?;
    }
    Ok(())
}

fn render_records(
    result: &FindResult,
    args: &FindArgs,
    palette: &crate::find::color::Palette,
    stdout: &mut dyn Write,
    _stderr: &mut dyn Write,
) -> std::io::Result<()> {
    let term_width = terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80);

    for (i, doc) in result.matches.iter().enumerate() {
        if i > 0 {
            writeln!(stdout)?;
            let sep = "─".repeat(term_width.saturating_sub(1).min(78));
            writeln!(
                stdout,
                "{}{}{}",
                palette.separator.render(),
                sep,
                palette.separator.render_reset()
            )?;
            writeln!(stdout)?;
        }
        render_record(doc, &args.col, term_width, palette, stdout)?;
    }

    if result.truncated {
        writeln!(stdout)?;
        writeln!(
            stdout,
            "{}… {} of {} (--no-limit for all){}",
            palette.footer.render(),
            result.returned,
            result.total,
            palette.footer.render_reset()
        )?;
    }
    Ok(())
}

fn render_record(
    doc: &vault_core::DocumentSummary,
    cols: &[String],
    term_width: usize,
    palette: &crate::find::color::Palette,
    stdout: &mut dyn Write,
) -> std::io::Result<()> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    pairs.push(("path".to_string(), doc.path.as_str().to_string()));

    let fm_object = doc.frontmatter.as_ref().and_then(|fm| fm.as_object());

    let field_iter: Vec<String> = if cols.is_empty() {
        fm_object
            .map(|obj| obj.keys().cloned().collect())
            .unwrap_or_default()
    } else {
        cols.to_vec()
    };

    for field in &field_iter {
        if let Some(value) = fm_object.and_then(|obj| obj.get(field)) {
            pairs.push((field.clone(), json_value_to_display_string(value)));
        }
    }

    let key_width = pairs.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let value_width = term_width.saturating_sub(key_width + 1).max(20);

    for (key, value) in &pairs {
        let mut first_line = true;
        for line in wrap_value(value, value_width) {
            if first_line {
                writeln!(
                    stdout,
                    "{}{:<width$}{} {}",
                    palette.key.render(),
                    key,
                    palette.key.render_reset(),
                    line,
                    width = key_width
                )?;
                first_line = false;
            } else {
                writeln!(stdout, "{:<width$} {}", "", line, width = key_width)?;
            }
        }
    }
    Ok(())
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
                "vault find: --col field '{}' is not present in any matching document",
                col
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
        writeln!(stderr, "vault find: --col is ignored with --format paths")?;
    }
    Ok(())
}

fn wrap_value(value: &str, width: usize) -> Vec<String> {
    if value.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    for word in value.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.len() + 1 + word.len() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            out.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
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
        assert!(std::str::from_utf8(&stderr).unwrap().contains("2 of 5"));
    }

    fn sample_args() -> FindArgs {
        FindArgs {
            text: None,
            eq: vec![],
            r#in: vec![],
            not_in: vec![],
            has: vec![],
            missing: vec![],
            before: vec![],
            after: vec![],
            on: vec![],
            path: vec![],
            sort: None,
            desc: false,
            limit: 10,
            no_limit: false,
            starts_at: 1,
            format: None,
            col: vec![],
            color: crate::cli::ColorWhen::Auto,
            no_pager: false,
        }
    }

    #[test]
    fn json_format_emits_wrapper_object() {
        let result = sample_result();
        let mut stdout = Vec::new();
        let args = sample_args();
        render_json(&result, &args, None, None, 1, &mut stdout).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(parsed["total"], 2);
        assert_eq!(parsed["returned"], 2);
        assert_eq!(parsed["truncated"], false);
        assert_eq!(parsed["matches"][0]["path"], "a.md");
        assert_eq!(parsed["matches"][0]["frontmatter"]["type"], "note");
        assert!(parsed["sort"].is_null());
    }

    #[test]
    fn json_includes_sort_when_present() {
        let result = sample_result();
        let mut stdout = Vec::new();
        let args = sample_args();
        render_json(
            &result,
            &args,
            Some("created"),
            Some("desc"),
            11,
            &mut stdout,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(parsed["sort"]["field"], "created");
        assert_eq!(parsed["sort"]["direction"], "desc");
        assert_eq!(parsed["starts_at"], 11);
    }

    #[test]
    fn json_col_narrows_frontmatter() {
        let result = sample_result();
        let mut stdout = Vec::new();
        let mut args = sample_args();
        args.col = vec!["type".to_string()];
        render_json(&result, &args, None, None, 1, &mut stdout).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(parsed["matches"][0]["frontmatter"]["type"], "note");
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
        assert!(std::str::from_utf8(&stderr).unwrap().contains("2 of 5"));
    }

    #[test]
    fn records_format_emits_key_value_blocks() {
        let result = sample_result();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let args = sample_args();
        let palette = crate::find::color::Palette::none();
        render_records(&result, &args, &palette, &mut stdout, &mut stderr).unwrap();
        let text = std::str::from_utf8(&stdout).unwrap();
        assert!(text.contains("path"));
        assert!(text.contains("a.md"));
        assert!(text.contains("type"));
        assert!(text.contains("note"));
        assert!(text.contains("b.md"));
        assert!(text.contains("─") || text.contains("---"));
    }

    #[test]
    fn records_truncated_emits_footer() {
        let mut result = sample_result();
        result.total = 5;
        result.truncated = true;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let args = sample_args();
        let palette = crate::find::color::Palette::none();
        render_records(&result, &args, &palette, &mut stdout, &mut stderr).unwrap();
        let text = std::str::from_utf8(&stdout).unwrap();
        assert!(text.contains("2 of 5"));
    }

    #[test]
    fn col_absent_in_all_matches_warns_to_stderr() {
        let result = sample_result();
        let mut stderr = Vec::new();
        let cols = vec!["nonexistent_field".to_string()];
        warn_absent_cols(&result, &cols, &mut stderr).unwrap();
        let stderr_str = std::str::from_utf8(&stderr).unwrap();
        assert!(stderr_str.contains("nonexistent_field"));
    }

    #[test]
    fn col_with_paths_format_warns() {
        let mut stderr = Vec::new();
        let cols = vec!["title".to_string()];
        warn_col_ignored_on_paths(&cols, crate::cli::FindFormat::Paths, &mut stderr).unwrap();
        let stderr_str = std::str::from_utf8(&stderr).unwrap();
        assert!(stderr_str.contains("--col is ignored with --format paths"));
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
        let palette = crate::find::color::Palette::none();
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
        let palette = crate::find::color::Palette::ansi();
        render_records(&result, &args, &palette, &mut stdout, &mut stderr).unwrap();
        let text = std::str::from_utf8(&stdout).unwrap();
        assert!(
            text.contains("\x1b["),
            "expected ANSI escapes, got: {}",
            text
        );
    }
}
