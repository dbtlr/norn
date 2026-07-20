//! Record-block line writers, ported from the donor `src/output/primitives.rs`
//! (retired tree). `find --format records` needs [`count_line`],
//! [`record_block`], and [`separator`]; `validate --format records` adds
//! [`status_headline`], [`severity_tally`], and [`tally_group`] (ported with the
//! first verb that emits them, NRN-381).
//!
//! Non-tty parity note: the parity harness runs piped, so the palette is `off`
//! (every style a no-op) and `term_width` is 80 with separators capped at 60 —
//! see the `find` render layer for where those are set.

use std::io::{self, Write};

use super::glyphs::{self, Glyph};
use super::palette::Palette;

/// A status headline: the text followed by a `…` ellipsis, the whole line in
/// `dim`, one newline. The validate records header ("running N rules across M
/// documents…").
pub fn status_headline(out: &mut dyn Write, p: &Palette, text: &str) -> io::Result<()> {
    write!(out, "{}{text}…{}", p.dim.render(), p.dim.render_reset())?;
    writeln!(out)
}

/// Severity tally: a three-line block (pass / warn / err); zero rows elided.
/// Right-aligned counts. If all three are zero, a single "0 {noun} pass" row is
/// emitted so the caller still shows a "the command ran" signal.
pub fn severity_tally(
    out: &mut dyn Write,
    p: &Palette,
    pass: usize,
    warn: usize,
    err: usize,
    noun: &str,
) -> io::Result<()> {
    let ascii = glyphs::use_ascii();
    let max_count = pass.max(warn).max(err);
    let w = max_count.to_string().len();

    let emit_pass = pass > 0 || (warn == 0 && err == 0);
    if emit_pass {
        let g = glyphs::render(Glyph::Pass, ascii);
        writeln!(
            out,
            "  {}{g}{}  {pass:>w$} {noun} pass",
            p.moss.render(),
            p.moss.render_reset(),
        )?;
    }
    if warn > 0 {
        let g = glyphs::render(Glyph::Warn, ascii);
        let label = if warn == 1 { "warning" } else { "warnings" };
        writeln!(
            out,
            "  {}{g}{}  {warn:>w$} {label}",
            p.amber.render(),
            p.amber.render_reset(),
        )?;
    }
    if err > 0 {
        let g = glyphs::render(Glyph::Err, ascii);
        let label = if err == 1 { "error" } else { "errors" };
        writeln!(
            out,
            "  {}{g}{}  {err:>w$} {label}",
            p.rune.render(),
            p.rune.render_reset(),
        )?;
    }
    Ok(())
}

/// A `header` section line (4-indent, `section` style) followed by aligned
/// `label ···· count` rows (4-indent label, `·` leaders filling to `term_width`,
/// right-aligned count in `thread`). Header omitted when empty; the whole call is
/// a no-op when `rows` is empty (the caller skips it).
pub fn tally_group(
    out: &mut dyn Write,
    p: &Palette,
    header: &str,
    rows: &[(&str, usize)],
    term_width: usize,
) -> io::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    if !header.is_empty() {
        writeln!(
            out,
            "  {}{header}{}",
            p.section.render(),
            p.section.render_reset(),
        )?;
    }
    let label_w = rows
        .iter()
        .map(|(l, _)| l.chars().count())
        .max()
        .unwrap_or(0)
        + 2;
    let count_w = rows
        .iter()
        .map(|(_, c)| c.to_string().chars().count())
        .max()
        .unwrap_or(1);

    // Row prefix is 4-indent + label-col + count-col + 2 spaces between leader
    // and count. Remaining width is the leader. Floor at 3 dots so narrow
    // terminals stay legible.
    let prefix_w = 4 + label_w + count_w + 2;
    let leader_w = term_width.saturating_sub(prefix_w).max(3);
    let leader: String = "·".repeat(leader_w);

    for (label, count) in rows {
        writeln!(
            out,
            "    {l_start}{label:<label_w$}{l_end}{d_start}{leader}{d_end}  {t_start}{count:>count_w$}{t_end}",
            l_start = p.label.render(),
            l_end = p.label.render_reset(),
            d_start = p.dim.render(),
            d_end = p.dim.render_reset(),
            t_start = p.thread.render(),
            t_end = p.thread.render_reset(),
        )?;
    }
    Ok(())
}

/// Count line: `"{total} {noun}"`, plus ` {·} showing {start}–{end}` when a
/// window is shown (`0 < returned < total`). Entire line in `dim`. One newline.
pub fn count_line(
    out: &mut dyn Write,
    p: &Palette,
    total: usize,
    returned: usize,
    starts_at: usize,
    noun: &str,
) -> io::Result<()> {
    let sep = glyphs::render(Glyph::Sep, glyphs::use_ascii());
    write!(out, "{}{total} {noun}", p.dim.render())?;
    if returned > 0 && returned < total {
        let end = starts_at + returned - 1;
        write!(out, " {sep} showing {starts_at}–{end}")?;
    }
    write!(out, "{}", p.dim.render_reset())?;
    writeln!(out)
}

/// One labelled field row of a record block.
pub struct Field<'a> {
    pub label: &'a str,
    pub value: &'a str,
    pub highlight: bool,
}

/// Record block: an optional column-0 header, then 2-indented `label  value`
/// rows. Label column width = `max(label.len()) + 2`; long values word-wrap into
/// the value column (over-long words force-broken). `highlight` renders the
/// value in `thread` rather than `bone`.
pub fn record_block(
    out: &mut dyn Write,
    p: &Palette,
    header: Option<&str>,
    fields: &[Field<'_>],
    term_width: usize,
) -> io::Result<()> {
    if let Some(h) = header {
        writeln!(out, "{}{h}{}", p.header.render(), p.header.render_reset())?;
    }
    if fields.is_empty() {
        return Ok(());
    }
    let label_w = fields.iter().map(|f| f.label.len()).max().unwrap_or(0) + 2;
    let value_w = term_width.saturating_sub(2 + label_w).max(20);

    for f in fields {
        let val_style = if f.highlight { &p.thread } else { &p.bone };
        let wrapped = wrap_value(f.value, value_w);
        for (i, line) in wrapped.iter().enumerate() {
            if i == 0 {
                writeln!(
                    out,
                    "  {l_start}{label:<label_w$}{l_end}{v_start}{line}{v_end}",
                    l_start = p.label.render(),
                    label = f.label,
                    l_end = p.label.render_reset(),
                    v_start = val_style.render(),
                    v_end = val_style.render_reset(),
                )?;
            } else {
                writeln!(
                    out,
                    "  {pad:<label_w$}{v_start}{line}{v_end}",
                    pad = "",
                    v_start = val_style.render(),
                    v_end = val_style.render_reset(),
                )?;
            }
        }
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
        if word.chars().count() > width {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            out.extend(chunk_str(word, width));
            continue;
        }
        if current.is_empty() {
            current.push_str(word);
        } else if current.chars().count() + 1 + word.chars().count() <= width {
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

fn chunk_str(s: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut count = 0;
    for c in s.chars() {
        current.push(c);
        count += 1;
        if count >= width {
            out.push(std::mem::take(&mut current));
            count = 0;
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Separator bar between records: `─` repeated `min(term_width, 60)` times, in
/// `dim`, one newline.
pub fn separator(out: &mut dyn Write, p: &Palette, term_width: usize) -> io::Result<()> {
    let width = term_width.min(60);
    let bar: String = "─".repeat(width);
    writeln!(out, "{}{}{}", p.dim.render(), bar, p.dim.render_reset())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_line_full_set_omits_window() {
        let mut out = Vec::new();
        count_line(&mut out, &Palette::off(), 3, 3, 1, "documents").unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "3 documents\n");
    }

    #[test]
    fn count_line_windowed_shows_range() {
        let mut out = Vec::new();
        count_line(&mut out, &Palette::off(), 23, 10, 1, "documents").unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "23 documents · showing 1–10\n"
        );
    }

    #[test]
    fn record_block_emits_header_then_indented_fields() {
        let mut out = Vec::new();
        let fields = [
            Field {
                label: "type",
                value: "note",
                highlight: false,
            },
            Field {
                label: "status",
                value: "backlog",
                highlight: false,
            },
        ];
        record_block(&mut out, &Palette::off(), Some("tasks/foo.md"), &fields, 80).unwrap();
        let lines: Vec<String> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        assert_eq!(lines[0], "tasks/foo.md");
        assert_eq!(lines[1], "  type    note");
        assert_eq!(lines[2], "  status  backlog");
    }

    #[test]
    fn separator_caps_at_60() {
        let mut out = Vec::new();
        separator(&mut out, &Palette::off(), 200).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s.chars().filter(|c| *c == '─').count(), 60);
    }

    // ── status_headline (ported from the donor primitives suite) ─────────────

    #[test]
    fn status_headline_writes_text_then_ellipsis_and_newline() {
        let mut out = Vec::new();
        status_headline(&mut out, &Palette::off(), "validating .norn/config.yaml").unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "validating .norn/config.yaml…\n"
        );
    }

    #[test]
    fn status_headline_on_palette_wraps_with_dim_ansi() {
        let mut out = Vec::new();
        status_headline(&mut out, &Palette::on(), "x").unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("\x1b["), "expected ANSI: {s:?}");
        assert!(s.contains("x…"));
    }

    // ── severity_tally (ported from the donor primitives suite) ──────────────

    #[test]
    fn severity_tally_pure_pass_shows_only_check_row() {
        let mut out = Vec::new();
        severity_tally(&mut out, &Palette::off(), 100, 0, 0, "documents").unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("100 documents pass"));
        assert!(!s.contains("warnings"));
        assert!(!s.contains("errors"));
    }

    #[test]
    fn severity_tally_mixed_shows_all_nonzero_rows_in_order() {
        let mut out = Vec::new();
        severity_tally(&mut out, &Palette::off(), 698, 71, 11, "documents").unwrap();
        let s = String::from_utf8(out).unwrap();
        let pass_pos = s.find("698 documents pass").unwrap();
        let warn_pos = s.find("71 warnings").unwrap();
        let err_pos = s.find("11 errors").unwrap();
        assert!(
            pass_pos < warn_pos && warn_pos < err_pos,
            "order pass→warn→err"
        );
    }

    #[test]
    fn severity_tally_elides_zero_rows() {
        let mut out = Vec::new();
        severity_tally(&mut out, &Palette::off(), 698, 0, 11, "documents").unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("698 documents pass"));
        assert!(!s.contains("warnings"));
        assert!(s.contains("11 errors"));
    }

    #[test]
    fn severity_tally_all_zero_emits_zero_pass_row() {
        let mut out = Vec::new();
        severity_tally(&mut out, &Palette::off(), 0, 0, 0, "documents").unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("0 documents pass"));
    }

    #[test]
    fn severity_tally_singular_warning_and_error_nouns() {
        let mut out = Vec::new();
        severity_tally(&mut out, &Palette::off(), 100, 1, 1, "documents").unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("1 warning"));
        assert!(!s.contains("1 warnings"));
        assert!(s.contains("1 error"));
        assert!(!s.contains("1 errors"));
    }

    // ── tally_group (ported from the donor primitives suite) ─────────────────

    #[test]
    fn tally_group_emits_header_and_rows() {
        let mut out = Vec::new();
        let rows = [("missing-required-field", 8), ("document-misrouted", 3)];
        tally_group(&mut out, &Palette::off(), "by code", &rows, 80).unwrap();
        let s = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines[0], "  by code");
        assert!(lines[1].starts_with("    missing-required-field"));
        assert!(lines[1].ends_with("  8"));
        assert!(
            lines[1].contains("··"),
            "expected leader dots: {:?}",
            lines[1]
        );
        assert!(lines[2].starts_with("    document-misrouted"));
        assert!(lines[2].ends_with("  3"));
    }

    #[test]
    fn tally_group_right_aligns_counts_to_widest() {
        let mut out = Vec::new();
        let rows = [("a", 5), ("b", 100), ("c", 12)];
        tally_group(&mut out, &Palette::off(), "by code", &rows, 80).unwrap();
        let s = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        // count_w = 3 (max of "5"=1, "100"=3, "12"=2); all right-aligned to 3.
        assert!(lines[1].ends_with("  5"), "row 1: {:?}", lines[1]);
        assert!(lines[2].ends_with("100"), "row 2: {:?}", lines[2]);
        assert!(lines[3].ends_with(" 12"), "row 3: {:?}", lines[3]);
    }

    #[test]
    fn tally_group_empty_rows_is_a_noop() {
        let mut out = Vec::new();
        tally_group(&mut out, &Palette::off(), "by code", &[], 80).unwrap();
        assert!(out.is_empty(), "empty rows must write nothing");
    }

    #[test]
    fn tally_group_uses_ansi_for_labels_and_leader_on_palette() {
        let mut out = Vec::new();
        let rows = [("x", 1)];
        tally_group(&mut out, &Palette::on(), "by code", &rows, 80).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("\x1b["), "expected ANSI: {s:?}");
    }
}
