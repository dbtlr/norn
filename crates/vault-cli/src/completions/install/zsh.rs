use anyhow::Result;
use camino::Utf8PathBuf;

use super::{home_dir, Installer, TargetPaths, MARKER_PREFIX, MARKER_SUFFIX};

pub(super) struct ZshInstaller;

impl Installer for ZshInstaller {
    fn shell_name(&self) -> &'static str {
        "zsh"
    }

    fn target_paths(&self) -> Result<TargetPaths> {
        let dir = if let Ok(zdotdir) = std::env::var("ZDOTDIR") {
            Utf8PathBuf::from(zdotdir)
        } else {
            home_dir()?
        };
        Ok(TargetPaths {
            primary: dir.join(".zshrc"),
            secondary: None,
        })
    }

    fn primary_content(&self, today: &str) -> Result<String> {
        Ok(format!(
            "{MARKER_PREFIX} (added by 'vault completions install' on {today}) >>>\neval \"$(vault completions init zsh)\"\n{MARKER_SUFFIX}",
        ))
    }

    fn secondary_content(&self) -> Result<Option<String>> {
        Ok(None)
    }
}
