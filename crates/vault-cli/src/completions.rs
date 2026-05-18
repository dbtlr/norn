use anyhow::Result;
use clap::CommandFactory;
use clap_complete::{generate, Shell};
use clap_mangen::Man;

use crate::cli::Cli;

/// Writes a shell completion script for `shell` to stdout, generated from
/// the existing clap `Cli` definition.
pub fn run_completions(shell: Shell) -> Result<()> {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    generate(shell, &mut cmd, name, &mut std::io::stdout());
    Ok(())
}

/// Writes a roff-format man page to stdout, generated from the existing
/// clap `Cli` definition.
pub fn run_manpage() -> Result<()> {
    let cmd = Cli::command();
    let man = Man::new(cmd);
    man.render(&mut std::io::stdout())?;
    Ok(())
}
