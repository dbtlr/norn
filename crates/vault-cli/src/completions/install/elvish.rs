use anyhow::Result;

use super::{xdg_config_home, Installer, TargetPaths, MARKER_PREFIX, MARKER_SUFFIX};

pub(super) struct ElvishInstaller;

impl Installer for ElvishInstaller {
    fn shell_name(&self) -> &'static str {
        "elvish"
    }

    fn target_paths(&self) -> Result<TargetPaths> {
        let xdg = xdg_config_home()?;
        Ok(TargetPaths {
            primary: xdg.join("elvish").join("rc.elv"),
            secondary: None,
        })
    }

    fn primary_content(&self, today: &str) -> Result<String> {
        Ok(format!(
            "{MARKER_PREFIX} (added by 'vault completions install' on {today}) >>>\neval (vault completions init elvish | slurp)\n{MARKER_SUFFIX}",
        ))
    }

    fn secondary_content(&self) -> Result<Option<String>> {
        Ok(None)
    }
}
