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

/// Render the short (`-h`) form of `model` to `out`.
///
/// `term_width` controls wrapping for the description line only — flag lines
/// in `-h` are one-liners per spec §1; they truncate at the value column,
/// never wrap.
#[allow(dead_code)]
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
    flags.iter().map(flag_label_width).max().unwrap_or(0) + 2
}

fn compute_aligned_column_borrowed(flags: &[&FlagEntry]) -> usize {
    flags.iter().map(|f| flag_label_width(f)).max().unwrap_or(0) + 2
}

fn flag_label_width(f: &FlagEntry) -> usize {
    flag_label(f).len()
}

/// Render the leading `-s, --long <PLACEHOLDER>` portion (without color).
pub(super) fn flag_label(f: &FlagEntry) -> String {
    let mut s = String::new();
    match (f.short, &f.long) {
        (Some(short), Some(long)) => {
            s.push_str(&format!("-{short}, --{long}"));
        }
        (Some(short), None) => {
            s.push_str(&format!("-{short}"));
        }
        (None, Some(long)) => {
            s.push_str(&format!("    --{long}"));
        }
        (None, None) => {
            // Positional: long is None; placeholder serves as the label.
        }
    }
    if let Some(vn) = &f.value_name {
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(&format!("<{vn}>"));
    }
    s
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
    globals.iter().map(global_label_width).max().unwrap_or(0) + 2
}

fn global_label_width(g: &GlobalEntry) -> usize {
    global_label(g).len()
}

fn global_label(g: &GlobalEntry) -> String {
    let mut s = String::new();
    match (g.short, &g.long) {
        (Some(short), Some(long)) => s.push_str(&format!("-{short}, --{long}")),
        (Some(short), None) => s.push_str(&format!("-{short}")),
        (None, Some(long)) => s.push_str(&format!("    --{long}")),
        (None, None) => {}
    }
    if let Some(vn) = &g.value_name {
        s.push(' ');
        s.push_str(&format!("<{vn}>"));
    }
    s
}

fn write_global_line(
    out: &mut dyn Write,
    palette: &Palette,
    g: &GlobalEntry,
    col: usize,
) -> io::Result<()> {
    let label = global_label(g);
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
                    },
                    FlagEntry {
                        short: None,
                        long: Some("all".to_string()),
                        value_name: None,
                        short_desc: "Return every document".to_string(),
                        long_desc: None,
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
}
