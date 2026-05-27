use anyhow::Result;
use clap::CommandFactory;
use clap_complete::{generate, Shell};

use super::{xdg_config_home, Installer, TargetPaths};
use crate::cli::Cli;

pub(super) struct FishInstaller;

impl Installer for FishInstaller {
    fn shell_name(&self) -> &'static str {
        "fish"
    }

    fn target_paths(&self) -> Result<TargetPaths> {
        let xdg = xdg_config_home()?;
        Ok(TargetPaths {
            primary: xdg.join("fish").join("completions").join("norn.fish"),
            secondary: None,
        })
    }

    fn primary_content(&self, _today: &str) -> Result<String> {
        // Fish loads completions from files. Generate the full script and
        // write it directly to the auto-loading completions dir.
        let mut cmd = Cli::command();
        let name = cmd.get_name().to_string();
        let mut buffer = Vec::new();
        generate(Shell::Fish, &mut cmd, name, &mut buffer);
        Ok(String::from_utf8(buffer)?)
    }

    fn secondary_content(&self) -> Result<Option<String>> {
        Ok(None)
    }

    fn uses_marker_block(&self) -> bool {
        false
    }
}
