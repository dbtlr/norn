//! Help rendering data model.

/// Which form of help is being rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpForm {
    /// `-h` — orient. Fits a screen. One-line descriptions.
    Short,
    /// `--help` — teach. Multi-page; paged. Per-flag prose where it earns it.
    Long,
}

/// A single flag or option entry.
#[derive(Debug, Clone)]
pub struct FlagEntry {
    /// Short flag like `-h`. `None` if the arg has only a long form.
    pub short: Option<char>,
    /// Long flag like `--text`. `None` if the arg has only a short form.
    pub long: Option<String>,
    /// Value placeholder like `<NEEDLE>` or `<FIELD:VALUE>`. `None` if the
    /// flag takes no value (e.g. `--all`).
    pub value_name: Option<String>,
    /// One-line description (shown in `-h` and as the lead line in `--help`).
    pub short_desc: String,
    /// Optional multi-paragraph prose. Rendered only in `--help`. `None`
    /// means the short description is the only text.
    pub long_desc: Option<String>,
    /// Possible enum values like `["json", "jsonl", "table"]`. Empty for
    /// free-form args. Shown in `--help` (long form) only.
    pub possible_values: Vec<String>,
}

/// A named group of flags, e.g. "Filter options", "Triage filters".
#[derive(Debug, Clone)]
pub struct FlagGroup {
    /// Display heading. Always rendered uppercase + dim bold by the renderer.
    pub heading: String,
    /// Flags in this group, in clap declaration order.
    pub flags: Vec<FlagEntry>,
}

/// A global option. Globals always render in one block (no collapse) with
/// one short line each.
#[derive(Debug, Clone)]
pub struct GlobalEntry {
    pub short: Option<char>,
    pub long: Option<String>,
    /// `None` if the global takes no value (e.g. `--verbose`).
    pub value_name: Option<String>,
    /// Constrained to ≤70 chars per the help-output v2 spec §2.2.
    pub short_desc: String,
}

/// Phase-deferred extras: examples, conceptual sections, live-examples hook.
/// Empty for every command in Phase 1.
#[derive(Debug, Clone, Default)]
pub struct HelpExtras {
    /// Phase 2: `Vec<(command_str, comment_str)>`.
    #[allow(dead_code)]
    pub canned_examples: Vec<(String, String)>,
    /// Phase 4: `Vec<(heading, body)>`.
    #[allow(dead_code)]
    pub conceptual_sections: Vec<(String, String)>,
    // Phase 3 adds a live-examples generator hook here.
}

/// Complete rendering input for one help invocation.
#[derive(Debug, Clone)]
pub struct HelpModel {
    /// e.g. `"vault"`, `"vault find"`, `"vault repair plan"`.
    pub command_path: String,
    /// One-line description (from clap `about`).
    pub about: String,
    /// Optional multi-line description (from clap `long_about`). Rendered
    /// only in `--help`.
    pub long_about: Option<String>,
    /// Positional arguments, in declaration order.
    pub positionals: Vec<FlagEntry>,
    /// Flag groups in display order (NOT clap declaration order). The
    /// extractor groups by `help_heading` and orders groups by first-seen.
    pub groups: Vec<FlagGroup>,
    /// Global options. Always rendered as one block after groups.
    pub globals: Vec<GlobalEntry>,
    /// Subcommand list for parent commands (e.g. `vault --help` lists
    /// `find`, `init`, `repair`, …). Empty for leaf commands.
    pub subcommands: Vec<(String, String)>, // (name, about)
    #[allow(dead_code)]
    pub extras: HelpExtras,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_entry_with_short_and_value() {
        let e = FlagEntry {
            short: Some('h'),
            long: Some("help".to_string()),
            value_name: None,
            short_desc: "Print help".to_string(),
            long_desc: None,
            possible_values: vec![],
        };
        assert_eq!(e.short, Some('h'));
        assert_eq!(e.long.as_deref(), Some("help"));
    }

    #[test]
    fn extras_default_is_empty() {
        let x = HelpExtras::default();
        assert!(x.canned_examples.is_empty());
        assert!(x.conceptual_sections.is_empty());
    }

    #[test]
    fn help_form_distinguishes_short_and_long() {
        assert_ne!(HelpForm::Short, HelpForm::Long);
    }
}
