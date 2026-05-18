//! Install completions into the user's shell config. Stub — fleshed out in Task 4.

use anyhow::{bail, Result};

use crate::cli::CompletionsInstallArgs;

/// Stub for `vault completions install`. Real implementation lands in Task 4.
pub fn run(_args: CompletionsInstallArgs) -> Result<InstallOutcome> {
    bail!("install not implemented yet")
}

/// Stub outcome type; expanded with real variants in Task 4.
#[derive(Debug)]
pub enum InstallOutcome {}

/// Stub renderer for the outcome. Replaced in Task 4.
pub fn render_outcome(_outcome: &InstallOutcome) -> String {
    String::new()
}
