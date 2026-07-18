//! Format dispatch + per-format renderers for `norn validate`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

use crate::core::Severity;
use crate::standards::{summarize, Finding};
use anyhow::Result;

use super::fix_hints::fix_hint_for;
use crate::cli::ValidateFormat;
use crate::output::glyphs::{self, Glyph};
use crate::output::palette::Palette;
use crate::output::primitives;

/// Top-level renderer. Dispatches by format and view (`summary_view` from
/// `args.summary`). `rules_count` and `total_docs` source the §6.1 status
/// headline ("running N rules across M documents…"). `findings` is the
/// post-triage-filter set; callers must filter before calling.
pub fn render(
    findings: &[Finding],
    summary_view: bool,
    rules_count: usize,
    total_docs: usize,
    format: ValidateFormat,
    palette: &Palette,
    stdout: &mut dyn Write,
) -> Result<()> {
    match format {
        ValidateFormat::Json => render_json(findings, summary_view, stdout),
        ValidateFormat::Jsonl => render_jsonl(findings, stdout),
        ValidateFormat::Paths => render_paths(findings, stdout),
        ValidateFormat::Records => {
            if summary_view {
                render_records_summary(findings, rules_count, total_docs, palette, stdout)
            } else {
                render_records_full(findings, rules_count, total_docs, palette, stdout)
            }
        }
    }
}

fn render_json(findings: &[Finding], summary_view: bool, stdout: &mut dyn Write) -> Result<()> {
    if summary_view {
        let summary = summarize(findings);
        writeln!(stdout, "{}", serde_json::to_string_pretty(&summary)?)?;
    } else {
        let payload = serde_json::json!({
            "total": findings.len(),
            "findings": findings,
        });
        writeln!(stdout, "{}", serde_json::to_string_pretty(&payload)?)?;
    }
    Ok(())
}

fn render_jsonl(findings: &[Finding], stdout: &mut dyn Write) -> Result<()> {
    for finding in findings {
        writeln!(stdout, "{}", serde_json::to_string(finding)?)?;
    }
    Ok(())
}

fn render_paths(findings: &[Finding], stdout: &mut dyn Write) -> Result<()> {
    let paths: BTreeSet<_> = findings.iter().map(|f| f.path.clone()).collect();
    for path in paths {
        writeln!(stdout, "{path}")?;
    }
    Ok(())
}

fn render_records_summary(
    findings: &[Finding],
    rules_count: usize,
    total_docs: usize,
    palette: &Palette,
    stdout: &mut dyn Write,
) -> Result<()> {
    let term_width = terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80);

    // Status headline.
    primitives::status_headline(
        stdout,
        palette,
        &format!("running {rules_count} rules across {total_docs} documents"),
    )?;
    writeln!(stdout)?;

    // Severity tally.
    let (warn, err) = count_severities(findings);
    let unique_doc_count: BTreeSet<_> = findings.iter().map(|f| &f.path).collect();
    let pass = total_docs.saturating_sub(unique_doc_count.len());
    primitives::severity_tally(stdout, palette, pass, warn, err, "documents")?;

    // by-code tally group (only when there are findings).
    if !findings.is_empty() {
        let summary = summarize(findings);
        writeln!(stdout)?;
        let rows: Vec<(&str, usize)> = summary
            .codes
            .iter()
            .map(|(k, v)| (k.as_str(), *v))
            .collect();
        primitives::tally_group(stdout, palette, "by code", &rows, term_width)?;
    }

    Ok(())
}

fn count_severities(findings: &[Finding]) -> (usize, usize) {
    let mut warn = 0;
    let mut err = 0;
    for f in findings {
        match f.severity {
            Severity::Warning => warn += 1,
            Severity::Error => err += 1,
        }
    }
    (warn, err)
}

fn render_records_full(
    findings: &[Finding],
    rules_count: usize,
    total_docs: usize,
    palette: &Palette,
    stdout: &mut dyn Write,
) -> Result<()> {
    primitives::status_headline(
        stdout,
        palette,
        &format!("running {rules_count} rules across {total_docs} documents"),
    )?;

    if findings.is_empty() {
        writeln!(stdout)?;
        primitives::severity_tally(stdout, palette, total_docs, 0, 0, "documents")?;
        return Ok(());
    }

    let grouped = group_by_code(findings);
    let ascii = glyphs::use_ascii();

    for (code, group) in &grouped {
        writeln!(stdout)?;
        let (glyph, style) = match group[0].severity {
            Severity::Warning => (glyphs::render(Glyph::Warn, ascii), &palette.amber),
            Severity::Error => (glyphs::render(Glyph::Err, ascii), &palette.rune),
        };
        writeln!(
            stdout,
            "{}{glyph}{} {}{code}{}",
            style.render(),
            style.render_reset(),
            palette.bone.render(),
            palette.bone.render_reset(),
        )?;
        for f in group {
            writeln!(
                stdout,
                "  {}{}{}",
                palette.bone.render(),
                f.path,
                palette.bone.render_reset(),
            )?;
            writeln!(
                stdout,
                "    {}{}{}",
                palette.dim.render(),
                f.message,
                palette.dim.render_reset(),
            )?;
            if let Some(hint) = fix_hint_for(code) {
                writeln!(
                    stdout,
                    "    {}fix:{} {}{hint}{}",
                    palette.thread.render(),
                    palette.thread.render_reset(),
                    palette.dim.render(),
                    palette.dim.render_reset(),
                )?;
            }
        }
    }

    // Footer count line: "{pass} documents pass · {N} findings shown"
    writeln!(stdout)?;
    let unique_doc_count: BTreeSet<_> = findings.iter().map(|f| &f.path).collect();
    let pass = total_docs.saturating_sub(unique_doc_count.len());
    let sep = glyphs::render(Glyph::Sep, ascii);
    writeln!(
        stdout,
        "{}{pass} documents pass {sep} {} findings shown{}",
        palette.dim.render(),
        findings.len(),
        palette.dim.render_reset(),
    )?;

    Ok(())
}

fn group_by_code(findings: &[Finding]) -> Vec<(String, Vec<&Finding>)> {
    let mut order: Vec<String> = Vec::new();
    let mut map: BTreeMap<String, Vec<&Finding>> = BTreeMap::new();
    for f in findings {
        if !map.contains_key(&f.code) {
            order.push(f.code.clone());
        }
        map.entry(f.code.clone()).or_default().push(f);
    }
    order
        .into_iter()
        .map(|code| {
            let group = map.remove(&code).unwrap();
            (code, group)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::standards::Finding;
    use serde_json::Value;

    fn sample_findings() -> Vec<Finding> {
        vec![
            Finding::frontmatter_required_missing(
                "notes/welcome.md".into(),
                Some("require-kind".to_string()),
                "kind".to_string(),
            ),
            Finding::frontmatter_required_missing(
                "notes/draft.md".into(),
                Some("require-kind".to_string()),
                "kind".to_string(),
            ),
            Finding::document_misrouted(
                "inbox/2026-05-12.md".into(),
                Some("notes-location".to_string()),
                vec!["notes/".to_string()],
            ),
        ]
    }

    #[test]
    fn render_json_wraps_findings_with_total() {
        let findings = sample_findings();
        let mut stdout = Vec::new();
        render_json(&findings, /* summary_view */ false, &mut stdout).unwrap();
        let parsed: Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(parsed["total"], 3);
        assert!(parsed["findings"].is_array());
        assert_eq!(parsed["findings"].as_array().unwrap().len(), 3);
        assert_eq!(
            parsed["findings"][0]["code"],
            "frontmatter-required-field-missing"
        );
    }

    #[test]
    fn render_json_summary_view_uses_summary_shape() {
        let findings = sample_findings();
        let mut stdout = Vec::new();
        render_json(&findings, /* summary_view */ true, &mut stdout).unwrap();
        let parsed: Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(parsed["findings"], 3); // findings field on Summary is the count, not an array
        assert!(parsed["codes"].is_object());
        assert_eq!(parsed["codes"]["frontmatter-required-field-missing"], 2);
        assert_eq!(parsed["codes"]["document-misrouted"], 1);
    }

    #[test]
    fn render_jsonl_emits_one_finding_per_line() {
        let findings = sample_findings();
        let mut stdout = Vec::new();
        render_jsonl(&findings, &mut stdout).unwrap();
        let s = std::str::from_utf8(&stdout).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 3);
        for line in &lines {
            let parsed: Value = serde_json::from_str(line).unwrap();
            assert!(parsed["path"].is_string());
            assert!(parsed["code"].is_string());
        }
    }

    #[test]
    fn render_paths_emits_unique_sorted_paths() {
        let findings = sample_findings();
        let mut stdout = Vec::new();
        render_paths(&findings, &mut stdout).unwrap();
        let s = std::str::from_utf8(&stdout).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(
            lines,
            vec!["inbox/2026-05-12.md", "notes/draft.md", "notes/welcome.md"]
        );
    }

    #[test]
    fn render_paths_dedupes_multiple_findings_on_one_doc() {
        let mut findings = sample_findings();
        // Add a second finding on welcome.md.
        findings.push(Finding::frontmatter_required_missing(
            "notes/welcome.md".into(),
            Some("require-title".to_string()),
            "title".to_string(),
        ));
        let mut stdout = Vec::new();
        render_paths(&findings, &mut stdout).unwrap();
        let s = std::str::from_utf8(&stdout).unwrap();
        let count = s.lines().filter(|l| *l == "notes/welcome.md").count();
        assert_eq!(count, 1, "welcome.md should appear once: {s:?}");
    }

    #[test]
    fn render_paths_no_ansi_ever() {
        let findings = sample_findings();
        let mut stdout = Vec::new();
        render_paths(&findings, &mut stdout).unwrap();
        assert!(!std::str::from_utf8(&stdout).unwrap().contains("\x1b["));
    }

    #[test]
    fn render_jsonl_no_ansi_ever() {
        let findings = sample_findings();
        let mut stdout = Vec::new();
        render_jsonl(&findings, &mut stdout).unwrap();
        assert!(!std::str::from_utf8(&stdout).unwrap().contains("\x1b["));
    }

    #[test]
    fn records_summary_emits_status_headline() {
        let findings = sample_findings();
        let mut stdout = Vec::new();
        let palette = Palette::off();
        render_records_summary(&findings, 12, 780, &palette, &mut stdout).unwrap();
        let s = std::str::from_utf8(&stdout).unwrap();
        let first_line = s.lines().next().unwrap();
        assert!(
            first_line.starts_with("running 12 rules across 780 documents"),
            "headline: {first_line:?}"
        );
        assert!(
            first_line.ends_with('…'),
            "headline ellipsis: {first_line:?}"
        );
    }

    #[test]
    fn records_summary_emits_severity_tally() {
        let findings = sample_findings();
        let mut stdout = Vec::new();
        let palette = Palette::off();
        render_records_summary(&findings, 12, 780, &palette, &mut stdout).unwrap();
        let s = std::str::from_utf8(&stdout).unwrap();
        // sample_findings touches 3 unique paths (welcome, draft, inbox) → 780 − 3 = 777 pass.
        assert!(s.contains("777 documents pass"), "expected pass row: {s:?}");
        assert!(s.contains("3 warnings"), "expected warning row: {s:?}");
    }

    #[test]
    fn records_summary_emits_by_code_tally_group() {
        let findings = sample_findings();
        let mut stdout = Vec::new();
        let palette = Palette::off();
        render_records_summary(&findings, 12, 780, &palette, &mut stdout).unwrap();
        let s = std::str::from_utf8(&stdout).unwrap();
        assert!(s.contains("  by code"));
        assert!(s.contains("frontmatter-required-field-missing"));
        assert!(s.contains("document-misrouted"));
    }

    #[test]
    fn records_summary_no_findings_emits_clean_message() {
        let findings: Vec<Finding> = vec![];
        let mut stdout = Vec::new();
        let palette = Palette::off();
        render_records_summary(&findings, 12, 780, &palette, &mut stdout).unwrap();
        let s = std::str::from_utf8(&stdout).unwrap();
        // Single tally row showing all docs pass.
        assert!(s.contains("780 documents pass"));
        // No "by code" group.
        assert!(!s.contains("by code"));
    }

    #[test]
    fn records_summary_color_off_no_ansi() {
        let findings = sample_findings();
        let mut stdout = Vec::new();
        render_records_summary(&findings, 12, 780, &Palette::off(), &mut stdout).unwrap();
        assert!(!std::str::from_utf8(&stdout).unwrap().contains("\x1b["));
    }

    #[test]
    fn records_summary_color_on_emits_ansi() {
        let findings = sample_findings();
        let mut stdout = Vec::new();
        render_records_summary(&findings, 12, 780, &Palette::on(), &mut stdout).unwrap();
        assert!(std::str::from_utf8(&stdout).unwrap().contains("\x1b["));
    }

    #[test]
    fn records_full_emits_status_headline() {
        let findings = sample_findings();
        let mut stdout = Vec::new();
        render_records_full(&findings, 12, 780, &Palette::off(), &mut stdout).unwrap();
        let s = std::str::from_utf8(&stdout).unwrap();
        assert!(s
            .lines()
            .next()
            .unwrap()
            .starts_with("running 12 rules across 780 documents"));
    }

    #[test]
    fn records_full_groups_by_code_with_severity_glyph_header() {
        let findings = sample_findings();
        let mut stdout = Vec::new();
        render_records_full(&findings, 12, 780, &Palette::off(), &mut stdout).unwrap();
        let s = std::str::from_utf8(&stdout).unwrap();
        // Glyph is "⚠" (utf) or "[warn]" (ascii). Use a permissive substring.
        assert!(
            s.contains("frontmatter-required-field-missing") && s.contains("document-misrouted"),
            "expected both code headers: {s:?}",
        );
    }

    #[test]
    fn records_full_path_at_2_indent_message_at_4_indent() {
        let findings = sample_findings();
        let mut stdout = Vec::new();
        render_records_full(&findings, 12, 780, &Palette::off(), &mut stdout).unwrap();
        let s = std::str::from_utf8(&stdout).unwrap();
        // 2-indent path under a code header.
        assert!(
            s.contains("\n  notes/welcome.md"),
            "expected 2-indent path: {s:?}"
        );
        // 4-indent message text.
        assert!(
            s.contains("\n    required frontmatter"),
            "expected 4-indent message: {s:?}"
        );
    }

    #[test]
    fn records_full_emits_fix_hint_for_known_codes() {
        let findings = sample_findings();
        let mut stdout = Vec::new();
        render_records_full(&findings, 12, 780, &Palette::off(), &mut stdout).unwrap();
        let s = std::str::from_utf8(&stdout).unwrap();
        assert!(
            s.contains("    fix: add the field"),
            "expected fix hint for required-field-missing: {s:?}"
        );
        assert!(
            s.contains("    fix: move the document"),
            "expected fix hint for document-misrouted: {s:?}"
        );
    }

    #[test]
    fn records_full_omits_fix_when_code_unknown() {
        // Build a finding with a synthetic unrecognized code via a GraphDiagnostic shape.
        use crate::core::Diagnostic;
        let finding = Finding::from_graph_diagnostic(
            "x.md".into(),
            Diagnostic::warning("not-a-real-code", "fake"),
        );
        let mut stdout = Vec::new();
        render_records_full(&[finding], 12, 780, &Palette::off(), &mut stdout).unwrap();
        let s = std::str::from_utf8(&stdout).unwrap();
        assert!(
            !s.contains("    fix:"),
            "unknown code should have no fix hint: {s:?}"
        );
    }

    #[test]
    fn records_full_footer_shows_pass_count_and_findings_shown() {
        let findings = sample_findings();
        let mut stdout = Vec::new();
        render_records_full(&findings, 12, 780, &Palette::off(), &mut stdout).unwrap();
        let s = std::str::from_utf8(&stdout).unwrap();
        // 780 total, 3 unique docs with findings → 777 pass. 3 findings shown.
        let footer = s.lines().last().unwrap();
        assert!(
            footer.contains("777 documents pass"),
            "footer pass count: {footer:?}"
        );
        assert!(
            footer.contains("3 findings shown"),
            "footer findings: {footer:?}"
        );
    }

    #[test]
    fn records_full_no_findings_collapses_to_clean_tally() {
        let findings: Vec<Finding> = vec![];
        let mut stdout = Vec::new();
        render_records_full(&findings, 12, 780, &Palette::off(), &mut stdout).unwrap();
        let s = std::str::from_utf8(&stdout).unwrap();
        assert!(s.contains("780 documents pass"));
        // No finding blocks → no "fix:" anywhere.
        assert!(!s.contains("fix:"));
    }

    #[test]
    fn records_full_color_on_emits_ansi() {
        let findings = sample_findings();
        let mut stdout = Vec::new();
        render_records_full(&findings, 12, 780, &Palette::on(), &mut stdout).unwrap();
        assert!(std::str::from_utf8(&stdout).unwrap().contains("\x1b["));
    }
}
