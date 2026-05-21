//! Build a [`HelpModel`] from a [`clap::Command`].

use clap::Command;

use super::model::{FlagEntry, FlagGroup, GlobalEntry, HelpExtras, HelpForm, HelpModel};

/// Heading used for a flag that has no `help_heading` annotation. Rendered
/// uppercased by the renderer (per spec §2.1).
const DEFAULT_FLAG_HEADING: &str = "Options";

/// Walk the given clap `Command` and produce a fully-populated `HelpModel`.
///
/// - `cmd_path` is the user-facing path string, e.g. `"vault find"`. The
///   caller assembles this from `BIN_NAME` and the subcommand chain.
/// - `form` selects whether long_about/long_help should be carried into the
///   model (the renderer also branches on `form`, but trimming early keeps
///   the model shape consistent).
#[allow(dead_code)]
pub fn build_model(cmd: &Command, cmd_path: &str, form: HelpForm) -> HelpModel {
    let about = cmd.get_about().map(|s| s.to_string()).unwrap_or_default();
    let long_about = match form {
        HelpForm::Long => cmd.get_long_about().map(|s| s.to_string()),
        HelpForm::Short => None,
    };

    let mut positionals: Vec<FlagEntry> = Vec::new();
    let mut groups: Vec<FlagGroup> = Vec::new();
    let mut globals: Vec<GlobalEntry> = Vec::new();

    for arg in cmd.get_arguments() {
        if arg.get_id() == "help" || arg.get_id() == "help_short" || arg.get_id() == "help_long" {
            // Help flags are not rendered in the model — the renderer adds
            // its own canonical help line in every group's tail block.
            continue;
        }
        let entry = flag_entry_from_arg(arg, form);
        if arg.is_positional() {
            positionals.push(entry);
            continue;
        }
        if arg.is_global_set() {
            globals.push(GlobalEntry {
                short: entry.short,
                long: entry.long,
                value_name: entry.value_name,
                short_desc: entry.short_desc,
            });
            continue;
        }
        let heading = arg
            .get_help_heading()
            .map(|s| s.to_string())
            .unwrap_or_else(|| DEFAULT_FLAG_HEADING.to_string());
        if let Some(g) = groups.iter_mut().find(|g| g.heading == heading) {
            g.flags.push(entry);
        } else {
            groups.push(FlagGroup {
                heading,
                flags: vec![entry],
            });
        }
    }

    let subcommands = cmd
        .get_subcommands()
        .filter(|sc| !sc.is_hide_set())
        .map(|sc| {
            (
                sc.get_name().to_string(),
                sc.get_about().map(|s| s.to_string()).unwrap_or_default(),
            )
        })
        .collect();

    HelpModel {
        command_path: cmd_path.to_string(),
        about,
        long_about,
        positionals,
        groups,
        globals,
        subcommands,
        extras: HelpExtras::default(),
    }
}

/// Map a single clap `Arg` to a `FlagEntry`, gating `long_desc` on `form`.
fn flag_entry_from_arg(arg: &clap::Arg, form: HelpForm) -> FlagEntry {
    let short = arg.get_short();
    let long = arg.get_long().map(|s| s.to_string());
    let value_name = arg
        .get_value_names()
        .and_then(|v| v.first())
        .map(|s| s.to_string());
    let short_desc = arg.get_help().map(|s| s.to_string()).unwrap_or_default();
    let long_desc = match form {
        HelpForm::Long => arg.get_long_help().map(|s| s.to_string()),
        HelpForm::Short => None,
    };
    FlagEntry {
        short,
        long,
        value_name,
        short_desc,
        long_desc,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{Arg, ArgAction, Command};

    fn sample_command() -> Command {
        Command::new("find")
            .about("Find documents")
            .long_about("Find documents in the vault.\n\nFull-text plus metadata filters.")
            .arg(
                Arg::new("text")
                    .long("text")
                    .value_name("NEEDLE")
                    .help("Full-text substring")
                    .help_heading("Filter options"),
            )
            .arg(
                Arg::new("limit")
                    .long("limit")
                    .value_name("N")
                    .help("Maximum matches")
                    .help_heading("Output"),
            )
            .arg(
                Arg::new("cwd")
                    .short('C')
                    .long("cwd")
                    .global(true)
                    .help("Run as if vault started in this directory")
                    .help_heading("Global options"),
            )
            .arg(
                Arg::new("all")
                    .long("all")
                    .action(ArgAction::SetTrue)
                    .help_heading("Filter options")
                    .help("Return every document"),
            )
    }

    #[test]
    fn extracts_about() {
        let cmd = sample_command();
        let model = build_model(&cmd, "vault find", HelpForm::Short);
        assert_eq!(model.about, "Find documents");
    }

    #[test]
    fn short_form_omits_long_about() {
        let cmd = sample_command();
        let model = build_model(&cmd, "vault find", HelpForm::Short);
        assert!(model.long_about.is_none());
    }

    #[test]
    fn long_form_includes_long_about() {
        let cmd = sample_command();
        let model = build_model(&cmd, "vault find", HelpForm::Long);
        assert!(model.long_about.as_deref().unwrap().contains("vault"));
    }

    #[test]
    fn groups_flags_by_help_heading() {
        let cmd = sample_command();
        let model = build_model(&cmd, "vault find", HelpForm::Short);
        let filter = model
            .groups
            .iter()
            .find(|g| g.heading == "Filter options")
            .expect("Filter options group");
        assert_eq!(filter.flags.len(), 2); // text + all
    }

    #[test]
    fn groups_preserve_first_seen_order() {
        let cmd = sample_command();
        let model = build_model(&cmd, "vault find", HelpForm::Short);
        let headings: Vec<&str> = model.groups.iter().map(|g| g.heading.as_str()).collect();
        assert_eq!(headings, vec!["Filter options", "Output"]);
    }

    #[test]
    fn globals_are_separated() {
        let cmd = sample_command();
        let model = build_model(&cmd, "vault find", HelpForm::Short);
        assert_eq!(model.globals.len(), 1);
        assert_eq!(model.globals[0].long.as_deref(), Some("cwd"));
        // The global should NOT appear inside a group.
        for g in &model.groups {
            assert!(g.flags.iter().all(|f| f.long.as_deref() != Some("cwd")));
        }
    }

    #[test]
    fn value_names_are_captured() {
        let cmd = sample_command();
        let model = build_model(&cmd, "vault find", HelpForm::Short);
        let text = model
            .groups
            .iter()
            .flat_map(|g| g.flags.iter())
            .find(|f| f.long.as_deref() == Some("text"))
            .expect("text flag");
        assert_eq!(text.value_name.as_deref(), Some("NEEDLE"));
    }
}
