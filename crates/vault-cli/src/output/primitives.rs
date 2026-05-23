//! Composable line writers per norn-cli-output.md §4.

use std::io::{self, Write};

use super::glyphs::{self, Glyph};
use super::palette::Palette;

/// Status headline: `{text}…` in `dim`. One trailing newline.
pub fn status_headline(out: &mut dyn Write, p: &Palette, text: &str) -> io::Result<()> {
    write!(out, "{}{text}…{}", p.dim.render(), p.dim.render_reset())?;
    writeln!(out)
}

/// Count line per norn-cli-output §4.1.
/// - `total == returned` (or empty) → `"{total} {noun}\n"` (no window).
/// - `returned < total` → `"{total} {noun} · showing {starts_at}–{end}\n"`
///   where end = starts_at + returned − 1.
/// - Both numbers and separator emitted in `dim`.
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

pub struct Field<'a> {
    pub label: &'a str,
    pub value: &'a str,
    pub highlight: bool,
}

/// Record block per norn-cli-output §4.3.
/// Optional header at column 0; pass `None` for header-less records where
/// every datum (including identity like a path) is a field row.
/// Then 2-indent field rows. Label column width = max(label.len()) + 2.
/// Long values wrap to value column on continuation lines (no label shown again).
/// Values containing words longer than the value column are force-broken at
/// the column boundary so they stay cell-shaped.
/// `highlight: true` renders the value in `thread`; otherwise `bone`.
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

pub fn separator(out: &mut dyn Write, p: &Palette, term_width: usize) -> io::Result<()> {
    let width = term_width.min(60);
    let bar: String = "─".repeat(width);
    writeln!(out, "{}{}{}", p.dim.render(), bar, p.dim.render_reset())
}

#[derive(Debug, Clone, Copy)]
pub enum NoteLabel {
    /// Reserved for future callers that emit informational notes (distinct from
    /// the `tip` variant, which is for next-step suggestions).
    #[allow(dead_code)]
    Note,
    Tip,
}

pub fn note_line(out: &mut dyn Write, p: &Palette, label: NoteLabel, body: &str) -> io::Result<()> {
    let label_str = match label {
        NoteLabel::Note => "note",
        NoteLabel::Tip => "tip",
    };
    writeln!(
        out,
        "{l_start}{label_str}:{l_end} {b_start}{body}{b_end}",
        l_start = p.thread.render(),
        l_end = p.thread.render_reset(),
        b_start = p.dim.render(),
        b_end = p.dim.render_reset(),
    )
}

/// Tally group per norn-cli-output §4.4.
///
/// Emits:
/// ```text
///   {header}                                  <- 2-indent, dim bold
///     {label}  ··········  {count}            <- 4-indent, label dim, leader dim, count thread
/// ```
///
/// Computes label column width = max(label.len()) + 2 and count column width =
/// max(count.to_string().len()) inside the function so callers don't have to.
/// Leader dots fill the gap between label and count up to `term_width`.
/// Header omitted (no line written) if `header` is empty.
/// Rows omitted entirely if `rows` is empty (caller is responsible for skipping
/// the call when there are no rows to emit).
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

    // Row prefix is 4-indent + label-col + count-col + 2 spaces between leader and count.
    // Remaining width is the leader. Floor at 3 dots so narrow terminals stay legible.
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

/// Severity tally per norn-cli-output §4.2.
/// Three-line block (pass / warn / err); zero rows elided. Right-aligned counts.
/// If all three are zero, emits a single "0 {noun} pass" row so the caller still
/// has a visible "the command ran" signal.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_headline_writes_text_then_ellipsis_and_newline() {
        let mut out = Vec::new();
        status_headline(&mut out, &Palette::off(), "validating .vault/config.yaml").unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "validating .vault/config.yaml…\n"
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
    fn count_line_starts_at_offset() {
        let mut out = Vec::new();
        count_line(&mut out, &Palette::off(), 23, 10, 11, "documents").unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "23 documents · showing 11–20\n"
        );
    }

    #[test]
    fn count_line_empty_set() {
        let mut out = Vec::new();
        count_line(&mut out, &Palette::off(), 0, 0, 1, "documents").unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "0 documents\n");
    }

    #[test]
    fn count_line_no_ansi_when_palette_off() {
        let mut out = Vec::new();
        count_line(&mut out, &Palette::off(), 23, 10, 1, "documents").unwrap();
        assert!(!String::from_utf8(out).unwrap().contains("\x1b["));
    }

    #[test]
    fn count_line_ansi_when_palette_on() {
        let mut out = Vec::new();
        count_line(&mut out, &Palette::on(), 23, 10, 1, "documents").unwrap();
        assert!(String::from_utf8(out).unwrap().contains("\x1b["));
    }

    #[test]
    fn severity_tally_pure_pass_shows_only_check_row() {
        let mut out = Vec::new();
        severity_tally(&mut out, &Palette::off(), 100, 0, 0, "documents").unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("✓"));
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

    #[test]
    fn record_block_emits_header_then_2_indent_fields() {
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
        let s = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines[0], "tasks/foo.md");
        // Label column width = max("type", "status") + 2 = 8 → "type    " / "status  ".
        assert_eq!(lines[1], "  type    note");
        assert_eq!(lines[2], "  status  backlog");
    }

    #[test]
    fn record_block_wraps_long_value_across_multiple_lines() {
        let mut out = Vec::new();
        let long = "the quick brown fox jumps over the lazy dog one more time";
        let fields = [Field {
            label: "k",
            value: long,
            highlight: false,
        }];
        record_block(&mut out, &Palette::off(), Some("h"), &fields, 30).unwrap();
        let s = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        // Header + at least 2 wrapped lines under "k".
        assert!(lines.len() >= 3, "expected wrap, got {:?}", lines);
        assert_eq!(lines[0], "h");
        // Continuation lines align past the label column.
        assert!(lines[2].starts_with("    "));
    }

    #[test]
    fn record_block_force_breaks_long_unbreakable_word() {
        // UUIDs and similar no-whitespace tokens used to overflow the value
        // column and let the terminal soft-wrap them into the key column.
        let mut out = Vec::new();
        let id = "a1b2c3d4-5e6f-7a8b-9c0d-1e2f3a4b5c6d";
        let fields = [Field {
            label: "id",
            value: id,
            highlight: false,
        }];
        record_block(&mut out, &Palette::off(), Some("h"), &fields, 30).unwrap();
        let s = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert!(
            lines.len() >= 3,
            "expected force-break wrap into multiple lines: {lines:?}"
        );
        // Continuation aligns to the value column (no overflow into key column).
        assert!(
            lines[2].starts_with("    "),
            "continuation should be indented past key column: {lines:?}"
        );
    }

    #[test]
    fn record_block_highlight_uses_thread_ansi_when_palette_on() {
        let mut out = Vec::new();
        let fields = [Field {
            label: "k",
            value: "v",
            highlight: true,
        }];
        record_block(&mut out, &Palette::on(), Some("h"), &fields, 80).unwrap();
        let s = String::from_utf8(out).unwrap();
        // Thread = ANSI 256 color 67 → escape "\x1b[38;5;67m"
        assert!(s.contains("\x1b[38;5;67m"), "expected thread ansi: {s:?}");
    }

    #[test]
    fn record_block_no_highlight_does_not_use_thread() {
        let mut out = Vec::new();
        let fields = [Field {
            label: "k",
            value: "v",
            highlight: false,
        }];
        record_block(&mut out, &Palette::on(), Some("h"), &fields, 80).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(
            !s.contains("\x1b[38;5;67m"),
            "unexpected thread ansi: {s:?}"
        );
    }

    #[test]
    fn record_block_single_field_emits_2_indent() {
        let mut out = Vec::new();
        let fields = [Field {
            label: "k",
            value: "v",
            highlight: false,
        }];
        record_block(&mut out, &Palette::off(), Some("h"), &fields, 80).unwrap();
        let s = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        // Single label "k": label_w = 1 + 2 = 3 → "k  v".
        assert_eq!(lines[1], "  k  v");
    }

    #[test]
    fn separator_caps_at_60_when_term_wider() {
        let mut out = Vec::new();
        separator(&mut out, &Palette::off(), 200).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s.chars().filter(|c| *c == '─').count(), 60);
        assert!(s.ends_with('\n'));
    }

    #[test]
    fn separator_respects_narrower_term() {
        let mut out = Vec::new();
        separator(&mut out, &Palette::off(), 40).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s.chars().filter(|c| *c == '─').count(), 40);
    }

    #[test]
    fn note_line_with_note_label() {
        let mut out = Vec::new();
        note_line(
            &mut out,
            &Palette::off(),
            NoteLabel::Note,
            "8 of 11 are auto-repairable",
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "note: 8 of 11 are auto-repairable\n"
        );
    }

    #[test]
    fn note_line_with_tip_label() {
        let mut out = Vec::new();
        note_line(
            &mut out,
            &Palette::off(),
            NoteLabel::Tip,
            "edit then run vault validate",
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "tip: edit then run vault validate\n"
        );
    }

    #[test]
    fn note_line_on_palette_emits_ansi() {
        let mut out = Vec::new();
        note_line(&mut out, &Palette::on(), NoteLabel::Tip, "body").unwrap();
        assert!(String::from_utf8(out).unwrap().contains("\x1b["));
    }

    #[test]
    fn tally_group_emits_header_and_rows() {
        let mut out = Vec::new();
        let rows = [("missing-required-field", 8), ("document-misrouted", 3)];
        tally_group(&mut out, &Palette::off(), "by code", &rows, 80).unwrap();
        let s = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines[0], "  by code");
        // Row format: "    {label:<label_w}{leader:·>leader_w}  {count:>count_w}"
        // label_w = max("missing-required-field"=22, "document-misrouted"=18) + 2 = 24.
        // count_w = max("8".len()=1, "3".len()=1) = 1.
        // First row: "    missing-required-field  " + leader dots + "  8"
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
        // count_w = 3 (max of "5"=1, "100"=3, "12"=2). All counts right-aligned to 3 chars.
        assert!(lines[1].ends_with("  5"), "row 1: {:?}", lines[1]);
        assert!(lines[2].ends_with("100"), "row 2: {:?}", lines[2]);
        assert!(lines[3].ends_with(" 12"), "row 3: {:?}", lines[3]);
    }

    #[test]
    fn tally_group_uses_dim_for_labels_and_leader_on_palette() {
        let mut out = Vec::new();
        let rows = [("x", 1)];
        tally_group(&mut out, &Palette::on(), "by code", &rows, 80).unwrap();
        let s = String::from_utf8(out).unwrap();
        // Palette::on() emits ANSI for dim and thread.
        assert!(s.contains("\x1b["), "expected ANSI: {s:?}");
    }

    #[test]
    fn tally_group_no_ansi_when_palette_off() {
        let mut out = Vec::new();
        let rows = [("x", 1)];
        tally_group(&mut out, &Palette::off(), "by code", &rows, 80).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(!s.contains("\x1b["));
    }

    #[test]
    fn tally_group_skips_header_when_empty() {
        let mut out = Vec::new();
        let rows = [("x", 1)];
        tally_group(&mut out, &Palette::off(), "", &rows, 80).unwrap();
        let s = String::from_utf8(out).unwrap();
        // First emitted line is a row (4-indent), not a header (2-indent).
        let first = s.lines().next().unwrap();
        assert!(
            first.starts_with("    "),
            "expected row first, got: {first:?}"
        );
        assert!(!s.starts_with("  by"), "header should be omitted: {s:?}");
    }

    #[test]
    fn tally_group_empty_rows_emits_nothing() {
        let mut out = Vec::new();
        let rows: [(&str, usize); 0] = [];
        tally_group(&mut out, &Palette::off(), "by code", &rows, 80).unwrap();
        assert!(
            out.is_empty(),
            "expected no output: {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    #[test]
    fn tally_group_narrow_terminal_keeps_minimum_leader() {
        // Even with a very narrow term_width, leave at least 3 dots between label and count
        // (lets the row stay legible if it overflows).
        let mut out = Vec::new();
        let rows = [("missing-required-field", 8)];
        tally_group(&mut out, &Palette::off(), "by code", &rows, 30).unwrap();
        let s = String::from_utf8(out).unwrap();
        let row = s.lines().nth(1).unwrap();
        assert!(
            row.contains("···"),
            "expected at least 3 leader dots: {row:?}"
        );
    }
}
