//! Build a [`HelpModel`] from a [`clap::Command`].
//!
//! Two things to note:
//! - Both the globals filter and the command-local args loop drop
//!   `is_hide_set()` args: the `--vault` global (ADR 0017) and a command-local
//!   hidden alias like `edit`'s `--ops-file` all parse but never surface in
//!   help, keeping every command's help output stable.
//! - The `live_examples_fn` extras hook is gone (see `model.rs`).

use clap::Command;

use super::model::{FlagEntry, FlagGroup, GlobalEntry, HelpExtras, HelpForm, HelpModel};

/// Heading used for a flag that has no `help_heading` annotation. Rendered
/// uppercased by the renderer (per spec §2.1).
const DEFAULT_FLAG_HEADING: &str = "Options";

/// Walk the given clap `Command` and produce a fully-populated `HelpModel`.
///
/// - `cmd_path` is the user-facing path string, e.g. `"norn find"`.
/// - `root` is the root `Cli::command()`. When `cmd` is a subcommand, global
///   options are read from `root` (clap only marks them `global_set` on the
///   declaring command, not on inherited copies in subcommands).
/// - `form` selects whether long_about/long_help should be carried into the
///   model.
pub fn build_model(cmd: &Command, root: &Command, cmd_path: &str, form: HelpForm) -> HelpModel {
    let about = cmd.get_about().map(|s| s.to_string()).unwrap_or_default();
    let long_about = match form {
        HelpForm::Long => cmd.get_long_about().map(|s| s.to_string()),
        HelpForm::Short => None,
    };

    let mut positionals: Vec<FlagEntry> = Vec::new();
    let mut groups: Vec<FlagGroup> = Vec::new();

    // Collect globals from the root command (the source of truth for global
    // args). Clap propagates globals to subcommands but `is_global_set()`
    // only returns `true` on the declaring command, not on inherited copies.
    let globals: Vec<GlobalEntry> = root
        .get_arguments()
        .filter(|a| {
            a.is_global_set()
                && !a.is_hide_set()
                && !matches!(
                    a.get_id().as_str(),
                    "help" | "help_short" | "help_long" | "version"
                )
        })
        .map(|a| {
            let entry = flag_entry_from_arg(a, form);
            GlobalEntry {
                short: entry.short,
                long: entry.long,
                value_name: entry.value_name,
                short_desc: entry.short_desc,
                // `long_desc` is populated only in the long form (the extractor
                // gates `flag_entry_from_arg`'s `long_desc` on `form`), so `-h`
                // renders the concise `short_desc` and `--help` the unclamped
                // full description when a global defines `long_help` (NRN-335).
                long_desc: entry.long_desc,
            }
        })
        .collect();

    // Walk this command's args. Globals were already collected from `root`
    // above; the `is_global_set()` skip below prevents double-collection.
    for arg in cmd.get_arguments() {
        if arg.get_id() == "help"
            || arg.get_id() == "help_short"
            || arg.get_id() == "help_long"
            || arg.get_id() == "version"
        {
            // Help and version flags are not rendered in the model.
            continue;
        }
        if arg.is_global_set() {
            continue;
        }
        if arg.is_hide_set() {
            // A command-local hidden arg (e.g. `edit`'s `--ops-file`, a
            // stdin-redirection alias) parses but must not surface in help —
            // the same rule the globals filter above already applies.
            continue;
        }
        let entry = flag_entry_from_arg(arg, form);
        if arg.is_positional() {
            positionals.push(entry);
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
        extras: HelpExtras {
            canned_examples: super::examples::examples_for(cmd_path),
            conceptual_sections: super::examples::conceptual_sections_for(cmd_path),
        },
        live_examples: Vec::new(),
    }
}

/// Map a single clap `Arg` to a `FlagEntry`, gating `long_desc` on `form`.
fn flag_entry_from_arg(arg: &clap::Arg, form: HelpForm) -> FlagEntry {
    let short = arg.get_short();
    let long = arg.get_long().map(|s| s.to_string());
    // SetTrue / SetFalse flags take no value — suppress any clap-generated
    // placeholder even if `get_value_names` returns one.
    let value_name = match arg.get_action() {
        clap::ArgAction::SetTrue | clap::ArgAction::SetFalse => None,
        _ => arg
            .get_value_names()
            .and_then(|v| v.first())
            .map(|s| s.to_string()),
    };
    let short_desc = arg.get_help().map(|s| s.to_string()).unwrap_or_default();
    let long_desc = match form {
        HelpForm::Long => arg.get_long_help().map(|s| s.to_string()),
        HelpForm::Short => None,
    };
    // Collect enum possible values (e.g. ["json", "jsonl", "table"] for
    // --format). These replace the clap-generated "[possible values: …]".
    let possible_values: Vec<String> = arg
        .get_possible_values()
        .iter()
        .filter(|pv| !pv.is_hide_set())
        .map(|pv| pv.get_name().to_string())
        .collect();
    FlagEntry {
        short,
        long,
        value_name,
        short_desc,
        long_desc,
        possible_values,
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
                    .help("Run as if norn started in this directory")
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
        let model = build_model(&cmd, &cmd, "norn find", HelpForm::Short);
        assert_eq!(model.about, "Find documents");
    }

    #[test]
    fn short_form_omits_long_about() {
        let cmd = sample_command();
        let model = build_model(&cmd, &cmd, "norn find", HelpForm::Short);
        assert!(model.long_about.is_none());
    }

    #[test]
    fn long_form_includes_long_about() {
        let cmd = sample_command();
        let model = build_model(&cmd, &cmd, "norn find", HelpForm::Long);
        assert!(model.long_about.as_deref().unwrap().contains("vault"));
    }

    #[test]
    fn groups_flags_by_help_heading() {
        let cmd = sample_command();
        let model = build_model(&cmd, &cmd, "norn find", HelpForm::Short);
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
        let model = build_model(&cmd, &cmd, "norn find", HelpForm::Short);
        let headings: Vec<&str> = model.groups.iter().map(|g| g.heading.as_str()).collect();
        assert_eq!(headings, vec!["Filter options", "Output"]);
    }

    #[test]
    fn globals_are_separated() {
        let cmd = sample_command();
        let model = build_model(&cmd, &cmd, "norn find", HelpForm::Short);
        assert_eq!(model.globals.len(), 1);
        assert_eq!(model.globals[0].long.as_deref(), Some("cwd"));
        for g in &model.groups {
            assert!(g.flags.iter().all(|f| f.long.as_deref() != Some("cwd")));
        }
    }

    #[test]
    fn hidden_global_is_excluded() {
        let cmd = Command::new("find").about("Find documents").arg(
            Arg::new("vault")
                .long("vault")
                .global(true)
                .hide(true)
                .help("Target a registered vault")
                .help_heading("Global options"),
        );
        let model = build_model(&cmd, &cmd, "norn find", HelpForm::Short);
        assert!(
            model
                .globals
                .iter()
                .all(|g| g.long.as_deref() != Some("vault")),
            "a hidden global must not appear in the model"
        );
    }

    #[test]
    fn hidden_command_local_arg_is_excluded() {
        // A hidden command-local arg (e.g. `edit`'s `--ops-file`) must not
        // surface in either the flag groups or the positionals — it parses,
        // but stays out of help (NRN-419).
        let cmd = Command::new("edit").about("Edit a document").arg(
            Arg::new("ops_file")
                .long("ops-file")
                .value_name("PATH")
                .hide(true)
                .help("Read the ops JSON array from a file"),
        );
        let model = build_model(&cmd, &cmd, "norn edit", HelpForm::Short);
        assert!(
            model
                .groups
                .iter()
                .flat_map(|g| g.flags.iter())
                .all(|f| f.long.as_deref() != Some("ops-file")),
            "a hidden command-local arg must not appear in the model"
        );
    }

    #[test]
    fn value_names_are_captured() {
        let cmd = sample_command();
        let model = build_model(&cmd, &cmd, "norn find", HelpForm::Short);
        let text = model
            .groups
            .iter()
            .flat_map(|g| g.flags.iter())
            .find(|f| f.long.as_deref() == Some("text"))
            .expect("text flag");
        assert_eq!(text.value_name.as_deref(), Some("NEEDLE"));
    }
}
