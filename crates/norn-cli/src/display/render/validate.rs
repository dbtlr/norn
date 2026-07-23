//! `validate` (NRN-409).

use std::io;

use serde_json::Value;

use crate::display::conversation::Conversation;
use crate::display::emit::render_outcome;
use crate::display::fix_hints::fix_hint_for;
use crate::display::format::Format;
use crate::display::output::ValidateView;
use crate::display::sink::Sink;
use crate::display::{EXIT_OK, EXIT_OPERATIONAL};
use crate::output::glyphs::{self, Glyph};

pub(crate) fn render_validate(
    view: ValidateView,
    format: Format,
    sink: &mut Sink<'_>,
    conv: &mut Conversation<'_>,
) -> i32 {
    // The findings arrive as the typed flat contract (ADR 0022). Serialize each
    // to a `Value` for the json / records / paths projections that read fields
    // by name; jsonl serializes each finding directly, one per line.
    let findings: Vec<Value> = view
        .report
        .findings
        .iter()
        .map(|f| serde_json::to_value(f).unwrap_or(Value::Null))
        .collect();

    let result: io::Result<i32> = (|| {
        match format {
            Format::Json => {
                if view.summary {
                    // `summary_json` is pretty-printed core-side (Some whenever
                    // `--summary` was set — the case that reaches here); emit it
                    // with exactly one trailing newline.
                    let body = view.report.summary_json.as_deref().unwrap_or("{}");
                    writeln!(sink.writer(), "{}", body.trim_end_matches('\n'))?;
                } else {
                    let payload = serde_json::json!({
                        "total": findings.len(),
                        "findings": findings,
                    });
                    writeln!(sink.writer(), "{}", serde_json::to_string_pretty(&payload)?)?;
                }
            }
            Format::Jsonl => {
                for finding in &view.report.findings {
                    writeln!(sink.writer(), "{}", serde_json::to_string(finding)?)?;
                }
            }
            Format::Paths => {
                let paths: std::collections::BTreeSet<&str> =
                    findings.iter().filter_map(|f| f["path"].as_str()).collect();
                for path in paths {
                    writeln!(sink.writer(), "{path}")?;
                }
            }
            Format::Records => {
                if view.summary {
                    render_validate_summary(
                        sink,
                        &findings,
                        view.report.rules_count,
                        view.report.total_docs,
                    )?;
                } else {
                    render_validate_full(
                        sink,
                        &findings,
                        view.report.rules_count,
                        view.report.total_docs,
                    )?;
                }
            }
            Format::Markdown => unreachable!("validate has no markdown format"),
        }
        Ok(if view.report.has_errors {
            EXIT_OPERATIONAL
        } else {
            EXIT_OK
        })
    })();

    render_outcome(result, conv.writer())
}

/// Count warning / error findings by their serialized `severity` field.
fn count_severities(findings: &[Value]) -> (usize, usize) {
    let mut warn = 0;
    let mut err = 0;
    for f in findings {
        match f["severity"].as_str() {
            Some("error") => err += 1,
            _ => warn += 1,
        }
    }
    (warn, err)
}

/// Distinct affected-document count (for the pass tally).
fn unique_doc_count(findings: &[Value]) -> usize {
    findings
        .iter()
        .filter_map(|f| f["path"].as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len()
}

/// `--summary` records: status headline, severity tally, and (when non-empty) a
/// by-code tally group.
fn render_validate_summary(
    sink: &mut Sink<'_>,
    findings: &[Value],
    rules_count: usize,
    total_docs: usize,
) -> io::Result<()> {
    let ascii = glyphs::use_ascii();
    sink.status_headline(
        &format!("running {rules_count} rules across {total_docs} documents"),
        ascii,
    )?;
    writeln!(sink.writer())?;

    let (warn, err) = count_severities(findings);
    let pass = total_docs.saturating_sub(unique_doc_count(findings));
    sink.severity_tally(pass, warn, err, "documents")?;

    if !findings.is_empty() {
        writeln!(sink.writer())?;
        // by-code counts, sorted by code (a BTreeMap).
        let mut by_code: std::collections::BTreeMap<&str, usize> =
            std::collections::BTreeMap::new();
        for f in findings {
            if let Some(code) = f["code"].as_str() {
                *by_code.entry(code).or_insert(0) += 1;
            }
        }
        let rows: Vec<(&str, usize)> = by_code.into_iter().collect();
        sink.tally_group("by code", &rows, ascii)?;
    }
    Ok(())
}

/// Full records: status headline; then per-code groups (first-occurrence order)
/// with a severity glyph header, each finding's path + message + optional fix
/// hint; then a pass/shown footer.
fn render_validate_full(
    sink: &mut Sink<'_>,
    findings: &[Value],
    rules_count: usize,
    total_docs: usize,
) -> io::Result<()> {
    let palette = *sink.palette();
    let ascii = glyphs::use_ascii();
    sink.status_headline(
        &format!("running {rules_count} rules across {total_docs} documents"),
        ascii,
    )?;

    if findings.is_empty() {
        writeln!(sink.writer())?;
        sink.severity_tally(total_docs, 0, 0, "documents")?;
        return Ok(());
    }

    for (code, group) in group_by_code(findings) {
        writeln!(sink.writer())?;
        let is_error = group
            .first()
            .and_then(|f| f["severity"].as_str())
            .is_some_and(|s| s == "error");
        let (glyph, style) = if is_error {
            (glyphs::render(Glyph::Err, ascii), &palette.rune)
        } else {
            (glyphs::render(Glyph::Warn, ascii), &palette.amber)
        };
        writeln!(
            sink.writer(),
            "{}{glyph}{} {}{code}{}",
            style.render(),
            style.render_reset(),
            palette.bone.render(),
            palette.bone.render_reset(),
        )?;
        for f in group {
            writeln!(
                sink.writer(),
                "  {}{}{}",
                palette.bone.render(),
                f["path"].as_str().unwrap_or(""),
                palette.bone.render_reset(),
            )?;
            writeln!(
                sink.writer(),
                "    {}{}{}",
                palette.dim.render(),
                f["message"].as_str().unwrap_or(""),
                palette.dim.render_reset(),
            )?;
            if let Some(hint) = fix_hint_for(code) {
                writeln!(
                    sink.writer(),
                    "    {}fix:{} {}{hint}{}",
                    palette.thread.render(),
                    palette.thread.render_reset(),
                    palette.dim.render(),
                    palette.dim.render_reset(),
                )?;
            }
        }
    }

    writeln!(sink.writer())?;
    let pass = total_docs.saturating_sub(unique_doc_count(findings));
    let sep = glyphs::render(Glyph::Sep, ascii);
    writeln!(
        sink.writer(),
        "{}{pass} documents pass {sep} {} findings shown{}",
        palette.dim.render(),
        findings.len(),
        palette.dim.render_reset(),
    )?;
    Ok(())
}

/// Group findings by `code`, preserving first-occurrence code order.
fn group_by_code(findings: &[Value]) -> Vec<(&str, Vec<&Value>)> {
    let mut order: Vec<&str> = Vec::new();
    let mut map: std::collections::BTreeMap<&str, Vec<&Value>> = std::collections::BTreeMap::new();
    for f in findings {
        let code = f["code"].as_str().unwrap_or("");
        if !map.contains_key(code) {
            order.push(code);
        }
        map.entry(code).or_default().push(f);
    }
    order
        .into_iter()
        .map(|code| {
            let group = map.remove(code).unwrap_or_default();
            (code, group)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::palette::Palette;
    use serde_json::json;

    // ── validate records renderers: input order is fixed per test so the
    // renderers are exercised deterministically despite the engine's order
    // nondeterminism ──────────────────────────────────────────────────────────

    /// A minimal finding value — the renderers read only these four fields.
    fn vf(code: &str, severity: &str, path: &str, message: &str) -> Value {
        json!({
            "code": code,
            "severity": severity,
            "path": path,
            "message": message,
        })
    }

    /// Three warning findings across three docs: two share a code (grouping),
    /// one is a second code.
    fn sample_validate_findings() -> Vec<Value> {
        vec![
            vf(
                "frontmatter-required-field-missing",
                "warning",
                "notes/welcome.md",
                "required frontmatter field is missing: kind",
            ),
            vf(
                "frontmatter-required-field-missing",
                "warning",
                "notes/draft.md",
                "required frontmatter field is missing: kind",
            ),
            vf(
                "document-misrouted",
                "warning",
                "inbox/2026-05-12.md",
                "document path is outside allowed rule locations",
            ),
        ]
    }

    fn full(findings: &[Value], palette: &Palette, total_docs: usize) -> String {
        let mut buf = Vec::new();
        {
            let mut sink = Sink::new(&mut buf, palette, 80);
            render_validate_full(&mut sink, findings, 12, total_docs).unwrap();
        }
        String::from_utf8(buf).unwrap()
    }

    fn summary(findings: &[Value], palette: &Palette, total_docs: usize) -> String {
        let mut buf = Vec::new();
        {
            let mut sink = Sink::new(&mut buf, palette, 80);
            render_validate_summary(&mut sink, findings, 12, total_docs).unwrap();
        }
        String::from_utf8(buf).unwrap()
    }

    // ── summary view ─────────────────────────────────────────────────────────

    #[test]
    fn validate_summary_emits_status_headline() {
        let s = summary(&sample_validate_findings(), &Palette::off(), 780);
        let first = s.lines().next().unwrap();
        assert!(
            first.starts_with("running 12 rules across 780 documents"),
            "headline: {first:?}"
        );
        assert!(first.ends_with('…'), "headline ellipsis: {first:?}");
    }

    #[test]
    fn validate_summary_emits_severity_tally() {
        let s = summary(&sample_validate_findings(), &Palette::off(), 780);
        // 3 unique docs with findings → 780 − 3 = 777 pass; all 3 are warnings.
        assert!(s.contains("777 documents pass"), "expected pass row: {s:?}");
        assert!(s.contains("3 warnings"), "expected warning row: {s:?}");
    }

    #[test]
    fn validate_summary_emits_by_code_tally_group() {
        let s = summary(&sample_validate_findings(), &Palette::off(), 780);
        assert!(s.contains("  by code"));
        assert!(s.contains("frontmatter-required-field-missing"));
        assert!(s.contains("document-misrouted"));
    }

    #[test]
    fn validate_summary_no_findings_emits_clean_tally_and_no_by_code() {
        let s = summary(&[], &Palette::off(), 780);
        assert!(s.contains("780 documents pass"));
        assert!(!s.contains("by code"));
    }

    #[test]
    fn validate_summary_color_off_no_ansi_color_on_ansi() {
        assert!(!summary(&sample_validate_findings(), &Palette::off(), 780).contains('\u{1b}'));
        assert!(summary(&sample_validate_findings(), &Palette::on(), 780).contains('\u{1b}'));
    }

    // ── full view ────────────────────────────────────────────────────────────

    #[test]
    fn validate_full_emits_status_headline() {
        let s = full(&sample_validate_findings(), &Palette::off(), 780);
        assert!(s
            .lines()
            .next()
            .unwrap()
            .starts_with("running 12 rules across 780 documents"));
    }

    #[test]
    fn validate_full_groups_by_code_with_both_headers() {
        let s = full(&sample_validate_findings(), &Palette::off(), 780);
        assert!(
            s.contains("frontmatter-required-field-missing") && s.contains("document-misrouted"),
            "expected both code headers: {s:?}"
        );
    }

    #[test]
    fn validate_full_path_at_2_indent_message_at_4_indent() {
        let s = full(&sample_validate_findings(), &Palette::off(), 780);
        assert!(
            s.contains("\n  notes/welcome.md"),
            "expected 2-indent path: {s:?}"
        );
        assert!(
            s.contains("\n    required frontmatter"),
            "expected 4-indent message: {s:?}"
        );
    }

    #[test]
    fn validate_full_emits_fix_hint_for_known_codes() {
        let s = full(&sample_validate_findings(), &Palette::off(), 780);
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
    fn validate_full_omits_fix_when_code_unknown() {
        let s = full(
            &[vf("not-a-real-code", "warning", "x.md", "fake")],
            &Palette::off(),
            780,
        );
        assert!(
            !s.contains("    fix:"),
            "unknown code has no fix hint: {s:?}"
        );
    }

    #[test]
    fn validate_full_footer_shows_pass_count_and_findings_shown() {
        let s = full(&sample_validate_findings(), &Palette::off(), 780);
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
    fn validate_full_no_findings_collapses_to_clean_tally() {
        let s = full(&[], &Palette::off(), 780);
        assert!(s.contains("780 documents pass"));
        assert!(!s.contains("fix:"));
    }

    #[test]
    fn validate_full_severity_selects_glyph_color_per_group() {
        // Under Palette::on(), a warning group header carries amber (ansi 178)
        // and an error group header carries rune (ansi 167) — locale-independent,
        // unlike the ✓/⚠/✗ glyph choice.
        let findings = vec![
            vf(
                "link-target-missing",
                "warning",
                "a.md",
                "link target not found: x",
            ),
            vf(
                "frontmatter-parse-failed",
                "error",
                "b.md",
                "frontmatter failed to parse",
            ),
        ];
        let s = full(&findings, &Palette::on(), 780);
        assert!(
            s.contains("\x1b[38;5;178m"),
            "expected amber on the warning group header: {s:?}"
        );
        assert!(
            s.contains("\x1b[38;5;167m"),
            "expected rune on the error group header: {s:?}"
        );
    }
}
