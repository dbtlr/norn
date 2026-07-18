use anyhow::Result;
use camino::Utf8PathBuf;

use super::{home_dir, Installer, TargetPaths, MARKER_PREFIX, MARKER_SUFFIX};

pub(super) struct PowershellInstaller;

impl Installer for PowershellInstaller {
    fn shell_name(&self) -> &'static str {
        "powershell"
    }

    fn target_paths(&self) -> Result<TargetPaths> {
        // POWERSHELL_PROFILE is a norn-specific env var for tests and
        // power users. Outside tests, real powershell users set their
        // profile via the powershell-managed $PROFILE variable; we use a
        // platform default to avoid shelling out to query it.
        if let Ok(profile) = std::env::var("POWERSHELL_PROFILE") {
            return Ok(TargetPaths {
                primary: Utf8PathBuf::from(profile),
                secondary: None,
            });
        }
        let home = home_dir()?;
        // Platform-default profile path (PowerShell Core / pwsh):
        //   Windows: $HOME\Documents\PowerShell\Microsoft.PowerShell_profile.ps1
        //   macOS/Linux: $HOME/.config/powershell/Microsoft.PowerShell_profile.ps1
        let path = if cfg!(target_os = "windows") {
            home.join("Documents")
                .join("PowerShell")
                .join("Microsoft.PowerShell_profile.ps1")
        } else {
            home.join(".config")
                .join("powershell")
                .join("Microsoft.PowerShell_profile.ps1")
        };
        Ok(TargetPaths {
            primary: path,
            secondary: None,
        })
    }

    fn primary_content(&self, today: &str) -> Result<String> {
        Ok(format!(
            "{MARKER_PREFIX} (added by 'norn completions install' on {today}) >>>\nnorn completions init powershell | Out-String | Invoke-Expression\n{MARKER_SUFFIX}",
        ))
    }

    fn secondary_content(&self) -> Result<Option<String>> {
        Ok(None)
    }
}
