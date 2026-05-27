use anyhow::Result;
use camino::Utf8PathBuf;
use clap::CommandFactory;
use clap_complete::generate;
use clap_complete_nushell::Nushell;

use super::{xdg_config_home, Installer, TargetPaths, MARKER_PREFIX, MARKER_SUFFIX};
use crate::cli::Cli;

pub(super) struct NushellInstaller;

impl Installer for NushellInstaller {
    fn shell_name(&self) -> &'static str {
        "nushell"
    }

    fn target_paths(&self) -> Result<TargetPaths> {
        let xdg = xdg_config_home()?;
        let script: Utf8PathBuf = xdg.join("nushell").join("completions").join("norn.nu");
        let config: Utf8PathBuf = xdg.join("nushell").join("config.nu");
        // Primary is the config.nu (the marker-block file). Secondary is the
        // script we write alongside it.
        Ok(TargetPaths {
            primary: config,
            secondary: Some(script),
        })
    }

    fn primary_content(&self, today: &str) -> Result<String> {
        // The marker block in config.nu sources the script file. We need to
        // reference the script path; resolve it the same way target_paths
        // does.
        let xdg = xdg_config_home()?;
        let script: Utf8PathBuf = xdg.join("nushell").join("completions").join("norn.nu");
        Ok(format!(
            "{MARKER_PREFIX} (added by 'norn completions install' on {today}) >>>\nsource {script}\n{MARKER_SUFFIX}",
        ))
    }

    fn secondary_content(&self) -> Result<Option<String>> {
        // Generate the nushell completion script.
        let mut cmd = Cli::command();
        let name = cmd.get_name().to_string();
        let mut buffer = Vec::new();
        generate(Nushell, &mut cmd, name, &mut buffer);
        Ok(Some(String::from_utf8(buffer)?))
    }
}
