use anyhow::Result;
use camino::Utf8PathBuf;

use super::{home_dir, Installer, TargetPaths, MARKER_PREFIX, MARKER_SUFFIX};

pub(super) struct BashInstaller;

impl Installer for BashInstaller {
    fn shell_name(&self) -> &'static str {
        "bash"
    }

    fn target_paths(&self) -> Result<TargetPaths> {
        // Respect $BASH_ENV if set, else prefer ~/.bash_profile on macOS (if it
        // exists), else ~/.bashrc.
        if let Ok(env_path) = std::env::var("BASH_ENV") {
            return Ok(TargetPaths {
                primary: Utf8PathBuf::from(env_path),
                secondary: None,
            });
        }
        let home = home_dir()?;
        if cfg!(target_os = "macos") {
            let bash_profile = home.join(".bash_profile");
            if bash_profile.exists() {
                return Ok(TargetPaths {
                    primary: bash_profile,
                    secondary: None,
                });
            }
        }
        Ok(TargetPaths {
            primary: home.join(".bashrc"),
            secondary: None,
        })
    }

    fn primary_content(&self, today: &str) -> Result<String> {
        Ok(format!(
            "{MARKER_PREFIX} (added by 'norn completions install' on {today}) >>>\neval \"$(norn completions init bash)\"\n{MARKER_SUFFIX}",
        ))
    }

    fn secondary_content(&self) -> Result<Option<String>> {
        Ok(None)
    }
}
