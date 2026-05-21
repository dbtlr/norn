//! Render a `HelpModel` to a byte buffer.
//!
//! Per the CLI Help Output v2 spec §3 and §3.1:
//! - flag names render in `thread`
//! - value placeholders render in `bone`
//! - section headers render in `dim` bold uppercase
//! - short descriptions render in `dim`
//! - `-h` uses a single global aligned column across all groups
//! - `--help` uses hanging indent (Task 6)

use std::io::{self, Write};

use super::model::{FlagEntry, GlobalEntry, HelpModel};
use crate::output::palette::Palette;

const GLOBAL_DESC_MAX: usize = 70;
const REPO_URL: &str = "https://github.com/dbtlr/vault-cli";

/// Abstracts over `FlagEntry` and `GlobalEntry` so `label()` can serve both.
trait LabelSource {
    fn short(&self) -> Option<char>;
    fn long(&self) -> Option<&str>;
    fn value_name(&self) -> Option<&str>;
}

impl LabelSource for FlagEntry {
    fn short(&self) -> Option<char> {
        self.short
    }
    fn long(&self) -> Option<&str> {
        self.long.as_deref()
    }
    fn value_name(&self) -> Option<&str> {
        self.value_name.as_deref()
    }
}

impl LabelSource for GlobalEntry {
    fn short(&self) -> Option<char> {
        self.short
    }
    fn long(&self) -> Option<&str> {
        self.long.as_deref()
    }
    fn value_name(&self) -> Option<&str> {
        self.value_name.as_deref()
    }
}

fn label<T: LabelSource>(item: &T) -> String {
    let mut s = String::new();
    match (item.short(), item.long()) {
        (Some(short), Some(long)) => s.push_str(&format!("-{short}, --{long}")),
        (Some(short), None) => s.push_str(&format!("-{short}")),
        (None, Some(long)) => s.push_str(&format!("    --{long}")),
        (None, None) => {}
    }
    if let Some(vn) = item.value_name() {
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(&format!("<{vn}>"));
    }
    s
}

/// Render the short (`-h`) form of `model` to `out`.
///
/// `term_width` controls wrapping for the description line only — flag lines
/// in `-h` are one-liners per spec §1; they truncate at the value column,
/// never wrap.
pub fn render_short(
    out: &mut dyn Write,
    model: &HelpModel,
    palette: &Palette,
    _term_width: usize,
) -> io::Result<()> {
    // Description line (bone-dim — rendered as dim).
    if !model.about.is_empty() {
        writeln!(
            out,
            "{}{}{}",
            palette.dim.render(),
            model.about,
            palette.dim.render_reset()
        )?;
        writeln!(out)?;
    }

    // USAGE line.
    write_section_header(out, palette, "USAGE")?;
    writeln!(
        out,
        "    {}{} [OPTIONS]{}{}",
        palette.bone.render(),
        model.command_path,
        if model.subcommands.is_empty() {
            ""
        } else {
            " <COMMAND>"
        },
        palette.bone.render_reset()
    )?;
    writeln!(out)?;

    // Positionals.
    if !model.positionals.is_empty() {
        write_section_header(out, palette, "ARGUMENTS")?;
        let col = compute_aligned_column(&model.positionals);
        for p in &model.positionals {
            write_flag_line_aligned(out, palette, p, col)?;
        }
        writeln!(out)?;
    }

    // Flag groups — single column across ALL groups (spec §3.1).
    let all_flags: Vec<&FlagEntry> = model.groups.iter().flat_map(|g| g.flags.iter()).collect();
    let col = compute_aligned_column_borrowed(&all_flags);
    for group in &model.groups {
        write_section_header(out, palette, &group.heading.to_uppercase())?;
        for f in &group.flags {
            write_flag_line_aligned(out, palette, f, col)?;
        }
        writeln!(out)?;
    }

    // Subcommands.
    if !model.subcommands.is_empty() {
        write_section_header(out, palette, "COMMANDS")?;
        let max_name = model
            .subcommands
            .iter()
            .map(|(n, _)| n.len())
            .max()
            .unwrap_or(0);
        for (name, about) in &model.subcommands {
            writeln!(
                out,
                "    {ts}{name:<width$}{te}  {ds}{about}{de}",
                ts = palette.thread.render(),
                name = name,
                width = max_name,
                te = palette.thread.render_reset(),
                ds = palette.dim.render(),
                about = about,
                de = palette.dim.render_reset(),
            )?;
        }
        writeln!(out)?;
    }

    // GLOBAL OPTIONS — full block, no collapse (spec §2.2).
    if !model.globals.is_empty() {
        write_section_header(out, palette, "GLOBAL OPTIONS")?;
        let col_g = compute_globals_column(&model.globals);
        for g in &model.globals {
            write_global_line(out, palette, g, col_g)?;
        }
        writeln!(out)?;
    }

    // Footer: pointer to long form.
    writeln!(
        out,
        "{}For full help, run `{} --help`.{}",
        palette.dim.render(),
        model.command_path,
        palette.dim.render_reset()
    )?;

    Ok(())
}

pub(super) fn write_section_header(
    out: &mut dyn Write,
    palette: &Palette,
    heading: &str,
) -> io::Result<()> {
    writeln!(
        out,
        "{}{}{}",
        palette.section.render(),
        heading,
        palette.section.render_reset()
    )
}

/// `(longest "flag + placeholder") + 2 spaces`.
fn compute_aligned_column(flags: &[FlagEntry]) -> usize {
    flags.iter().map(|f| flag_label(f).len()).max().unwrap_or(0) + 2
}

fn compute_aligned_column_borrowed(flags: &[&FlagEntry]) -> usize {
    flags.iter().map(|f| flag_label(f).len()).max().unwrap_or(0) + 2
}

/// Render the leading `-s, --long <PLACEHOLDER>` portion (without color).
pub(super) fn flag_label(f: &FlagEntry) -> String {
    label(f)
}

fn write_flag_line_aligned(
    out: &mut dyn Write,
    palette: &Palette,
    f: &FlagEntry,
    col: usize,
) -> io::Result<()> {
    let label = flag_label(f);
    let (flag_part, placeholder_part) = split_flag_and_placeholder(&label);
    let pad = col.saturating_sub(label.len());
    writeln!(
        out,
        "    {fs}{flag}{fe}{ps}{ph}{pe}{spaces}{ds}{desc}{de}",
        fs = palette.thread.render(),
        flag = flag_part,
        fe = palette.thread.render_reset(),
        ps = palette.bone.render(),
        ph = placeholder_part,
        pe = palette.bone.render_reset(),
        spaces = " ".repeat(pad),
        ds = palette.dim.render(),
        desc = f.short_desc,
        de = palette.dim.render_reset(),
    )
}

pub(super) fn split_flag_and_placeholder(label: &str) -> (&str, &str) {
    if let Some(idx) = label.find(" <") {
        (&label[..idx], &label[idx..])
    } else {
        (label, "")
    }
}

fn compute_globals_column(globals: &[GlobalEntry]) -> usize {
    globals.iter().map(|g| label(g).len()).max().unwrap_or(0) + 2
}

fn write_global_line(
    out: &mut dyn Write,
    palette: &Palette,
    g: &GlobalEntry,
    col: usize,
) -> io::Result<()> {
    let label = label(g);
    let (flag_part, placeholder_part) = split_flag_and_placeholder(&label);
    let pad = col.saturating_sub(label.len());
    // Constrain description per spec §2.2.
    let desc = if g.short_desc.len() > GLOBAL_DESC_MAX {
        format!("{}…", &g.short_desc[..GLOBAL_DESC_MAX.saturating_sub(1)])
    } else {
        g.short_desc.clone()
    };
    writeln!(
        out,
        "    {fs}{flag}{fe}{ps}{ph}{pe}{spaces}{ds}{desc}{de}",
        fs = palette.thread.render(),
        flag = flag_part,
        fe = palette.thread.render_reset(),
        ps = palette.bone.render(),
        ph = placeholder_part,
        pe = palette.bone.render_reset(),
        spaces = " ".repeat(pad),
        ds = palette.dim.render(),
        desc = desc,
        de = palette.dim.render_reset(),
    )
}

/// Render the long (`--help`) form of `model` to `out`.
///
/// Hanging-indent style for flags: flag on its own line, descriptions/prose
/// indented 8 spaces beneath. Globals still use the aligned column.
pub fn render_long(
    out: &mut dyn Write,
    model: &HelpModel,
    palette: &Palette,
    term_width: usize,
) -> io::Result<()> {
    // Description (one-line about).
    if !model.about.is_empty() {
        writeln!(
            out,
            "{}{}{}",
            palette.dim.render(),
            model.about,
            palette.dim.render_reset()
        )?;
        writeln!(out)?;
    }

    // Long about (multi-paragraph prose).
    if let Some(long) = &model.long_about {
        for paragraph in long.split("\n\n") {
            writeln!(
                out,
                "{}{}{}",
                palette.dim.render(),
                paragraph,
                palette.dim.render_reset()
            )?;
            writeln!(out)?;
        }
    }

    // USAGE.
    write_section_header(out, palette, "USAGE")?;
    writeln!(
        out,
        "    {}{} [OPTIONS]{}{}",
        palette.bone.render(),
        model.command_path,
        if model.subcommands.is_empty() {
            ""
        } else {
            " <COMMAND>"
        },
        palette.bone.render_reset()
    )?;
    writeln!(out)?;

    // Positionals — hanging indent (description on its own line).
    if !model.positionals.is_empty() {
        write_section_header(out, palette, "ARGUMENTS")?;
        for p in &model.positionals {
            write_flag_hanging(out, palette, p, term_width)?;
        }
    }

    // Flag groups — hanging indent.
    for group in &model.groups {
        write_section_header(out, palette, &group.heading.to_uppercase())?;
        for f in &group.flags {
            write_flag_hanging(out, palette, f, term_width)?;
        }
    }

    // Subcommands.
    if !model.subcommands.is_empty() {
        write_section_header(out, palette, "COMMANDS")?;
        let max_name = model
            .subcommands
            .iter()
            .map(|(n, _)| n.len())
            .max()
            .unwrap_or(0);
        for (name, about) in &model.subcommands {
            writeln!(
                out,
                "    {ts}{name:<width$}{te}  {ds}{about}{de}",
                ts = palette.thread.render(),
                name = name,
                width = max_name,
                te = palette.thread.render_reset(),
                ds = palette.dim.render(),
                about = about,
                de = palette.dim.render_reset(),
            )?;
        }
        writeln!(out)?;
    }

    // GLOBAL OPTIONS — aligned column (spec §3.1 — short lines use the column).
    if !model.globals.is_empty() {
        write_section_header(out, palette, "GLOBAL OPTIONS")?;
        let col_g = compute_globals_column(&model.globals);
        for g in &model.globals {
            write_global_line(out, palette, g, col_g)?;
        }
        writeln!(out)?;
    }

    // Footer: docs URL.
    writeln!(
        out,
        "{}Documentation: {}{}",
        palette.dim.render(),
        REPO_URL,
        palette.dim.render_reset()
    )?;

    Ok(())
}

fn write_flag_hanging(
    out: &mut dyn Write,
    palette: &Palette,
    f: &FlagEntry,
    _term_width: usize,
) -> io::Result<()> {
    // Flag line.
    let lbl = flag_label(f);
    let (flag_part, placeholder_part) = split_flag_and_placeholder(&lbl);
    writeln!(
        out,
        "    {fs}{flag}{fe}{ps}{ph}{pe}",
        fs = palette.thread.render(),
        flag = flag_part,
        fe = palette.thread.render_reset(),
        ps = palette.bone.render(),
        ph = placeholder_part,
        pe = palette.bone.render_reset(),
    )?;
    // Short description (always shown).
    if !f.short_desc.is_empty() {
        writeln!(
            out,
            "        {ds}{desc}{de}",
            ds = palette.dim.render(),
            desc = f.short_desc,
            de = palette.dim.render_reset(),
        )?;
    }
    // Long description (only when a flag earns one — spec §3.2).
    if let Some(long) = &f.long_desc {
        for paragraph in long.split("\n\n") {
            writeln!(
                out,
                "        {ds}{p}{de}",
                ds = palette.dim.render(),
                p = paragraph,
                de = palette.dim.render_reset(),
            )?;
        }
    }
    // Possible enum values (e.g. "Possible values: bash, zsh, fish, ...").
    if !f.possible_values.is_empty() {
        writeln!(
            out,
            "        {ds}Possible values: {vals}{de}",
            ds = palette.dim.render(),
            vals = f.possible_values.join(", "),
            de = palette.dim.render_reset(),
        )?;
    }
    writeln!(out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::help::model::{FlagEntry, FlagGroup, GlobalEntry, HelpExtras, HelpModel};
    use crate::output::palette::Palette;

    fn sample_model() -> HelpModel {
        HelpModel {
            command_path: "vault find".to_string(),
            about: "Find documents".to_string(),
            long_about: None,
            positionals: vec![],
            groups: vec![FlagGroup {
                heading: "Filter options".to_string(),
                flags: vec![
                    FlagEntry {
                        short: None,
                        long: Some("text".to_string()),
                        value_name: Some("NEEDLE".to_string()),
                        short_desc: "Full-text substring".to_string(),
                        long_desc: None,
                        possible_values: vec![],
                    },
                    FlagEntry {
                        short: None,
                        long: Some("all".to_string()),
                        value_name: None,
                        short_desc: "Return every document".to_string(),
                        long_desc: None,
                        possible_values: vec![],
                    },
                ],
            }],
            globals: vec![GlobalEntry {
                short: Some('C'),
                long: Some("cwd".to_string()),
                value_name: None,
                short_desc: "Run as if vault started in this directory".to_string(),
            }],
            subcommands: vec![],
            extras: HelpExtras::default(),
        }
    }

    fn render_to_string(model: &HelpModel) -> String {
        let palette = Palette::off();
        let mut buf = Vec::new();
        render_short(&mut buf, model, &palette, 100).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn renders_description_first() {
        let out = render_to_string(&sample_model());
        assert!(out.starts_with("Find documents\n"));
    }

    #[test]
    fn renders_usage_block() {
        let out = render_to_string(&sample_model());
        assert!(out.contains("USAGE\n"));
        assert!(out.contains("vault find [OPTIONS]"));
    }

    #[test]
    fn renders_group_heading_uppercased() {
        let out = render_to_string(&sample_model());
        assert!(out.contains("FILTER OPTIONS\n"));
        assert!(!out.contains("Filter options\n"));
    }

    #[test]
    fn renders_flag_with_placeholder() {
        let out = render_to_string(&sample_model());
        assert!(out.contains("--text <NEEDLE>"));
    }

    #[test]
    fn renders_globals_block_full() {
        let out = render_to_string(&sample_model());
        assert!(out.contains("GLOBAL OPTIONS\n"));
        assert!(out.contains("-C, --cwd"));
        assert!(out.contains("Run as if vault started in this directory"));
    }

    #[test]
    fn renders_long_form_footer_pointer() {
        let out = render_to_string(&sample_model());
        assert!(out.contains("For full help, run `vault find --help`."));
    }

    #[test]
    fn global_description_over_max_is_truncated() {
        let mut model = sample_model();
        model.globals[0].short_desc = "x".repeat(80);
        let out = render_to_string(&model);
        // Truncated to GLOBAL_DESC_MAX-1 chars plus the ellipsis.
        assert!(out.contains(&format!("{}…", "x".repeat(GLOBAL_DESC_MAX - 1))));
    }

    #[test]
    fn aligned_column_uses_global_longest() {
        // Two groups with very different flag lengths — the column must align
        // to the longest across BOTH groups.
        let model = HelpModel {
            command_path: "vault find".to_string(),
            about: String::new(),
            long_about: None,
            positionals: vec![],
            groups: vec![
                FlagGroup {
                    heading: "A".to_string(),
                    flags: vec![FlagEntry {
                        short: None,
                        long: Some("x".to_string()),
                        value_name: None,
                        short_desc: "short".to_string(),
                        long_desc: None,
                        possible_values: vec![],
                    }],
                },
                FlagGroup {
                    heading: "B".to_string(),
                    flags: vec![FlagEntry {
                        short: None,
                        long: Some("very-long-flag-name".to_string()),
                        value_name: Some("PLACEHOLDER".to_string()),
                        short_desc: "zebra".to_string(),
                        long_desc: None,
                        possible_values: vec![],
                    }],
                },
            ],
            globals: vec![],
            subcommands: vec![],
            extras: HelpExtras::default(),
        };
        let out = render_to_string(&model);
        let lines: Vec<&str> = out.lines().collect();
        let short_line = lines.iter().find(|l| l.contains("short")).unwrap();
        let long_line = lines.iter().find(|l| l.contains("zebra")).unwrap();
        let short_pos = short_line.find("short").unwrap();
        let long_pos = long_line.find("zebra").unwrap();
        assert_eq!(short_pos, long_pos, "descriptions must align across groups");
    }

    fn render_long_to_string(model: &HelpModel) -> String {
        let palette = Palette::off();
        let mut buf = Vec::new();
        render_long(&mut buf, model, &palette, 100).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn long_form_starts_with_about_then_long_about() {
        let mut model = sample_model();
        model.long_about =
            Some("Find documents in the vault.\n\nFull-text plus metadata.".to_string());
        let out = render_long_to_string(&model);
        // About first, then a blank line, then long_about.
        assert!(out.starts_with("Find documents\n"));
        assert!(out.contains("Find documents in the vault."));
        assert!(out.contains("Full-text plus metadata."));
    }

    #[test]
    fn long_form_uses_hanging_indent_for_flags() {
        let mut model = sample_model();
        model.groups[0].flags[0].long_desc =
            Some("Substring match against document body. Case-insensitive.".to_string());
        let out = render_long_to_string(&model);
        // Flag line stands alone; description is on the next line indented.
        let lines: Vec<&str> = out.lines().collect();
        let flag_idx = lines
            .iter()
            .position(|l| l.contains("--text <NEEDLE>"))
            .unwrap();
        let next = lines[flag_idx + 1];
        assert!(
            next.starts_with("        "),
            "hanging indent (8 spaces), got: {next:?}"
        );
        assert!(next.contains("Full-text substring"));
    }

    #[test]
    fn long_form_renders_long_desc_paragraphs_at_hanging_indent() {
        let mut model = sample_model();
        model.groups[0].flags[0].long_desc = Some(
            "First paragraph of long_desc body.\n\nSecond paragraph of long_desc body.".to_string(),
        );
        let out = render_long_to_string(&model);
        let lines: Vec<&str> = out.lines().collect();
        // short_desc comes first under the flag; long_desc paragraphs follow.
        let short_idx = lines
            .iter()
            .position(|l| l.contains("Full-text substring"))
            .expect("short_desc line");
        let first_para_idx = lines
            .iter()
            .position(|l| l.contains("First paragraph of long_desc body."))
            .expect("first long_desc paragraph");
        let second_para_idx = lines
            .iter()
            .position(|l| l.contains("Second paragraph of long_desc body."))
            .expect("second long_desc paragraph");
        assert!(
            short_idx < first_para_idx,
            "short_desc must come before long_desc"
        );
        assert!(
            first_para_idx < second_para_idx,
            "paragraphs render in order"
        );
        assert!(
            lines[first_para_idx].starts_with("        "),
            "first paragraph at 8-space indent, got: {:?}",
            lines[first_para_idx]
        );
        assert!(
            lines[second_para_idx].starts_with("        "),
            "second paragraph at 8-space indent, got: {:?}",
            lines[second_para_idx]
        );
    }

    #[test]
    fn long_form_renders_globals_with_aligned_column() {
        let out = render_long_to_string(&sample_model());
        // Globals still use the aligned column, not hanging indent.
        assert!(out.contains("GLOBAL OPTIONS\n"));
        let lines: Vec<&str> = out.lines().collect();
        let global_line = lines.iter().find(|l| l.contains("-C, --cwd")).unwrap();
        assert!(global_line.contains("Run as if vault started in this directory"));
    }

    #[test]
    fn long_form_footer_is_docs_pointer() {
        let out = render_long_to_string(&sample_model());
        // Phase 1 footer: a docs pointer line.
        assert!(out.to_lowercase().contains("documentation"));
        assert!(out.contains("github.com"));
    }
}
