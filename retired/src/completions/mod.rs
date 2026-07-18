//! Shell-completion and manpage generation for `norn completions`.
//!
//! Generates completion scripts and man pages straight off the derive-generated
//! clap `Cli`, so they never fall out of sync with the real command surface.
//! `run_init` writes a script to stdout; the `install` submodule places it in
//! the shell's completion directory. Driven by the `Completions` arm in
//! `lib.rs`; a `LocalOnly` command with no MCP twin.

use anyhow::Result;
use clap::CommandFactory;
use clap_complete::generate;
use clap_mangen::Man;

use crate::cli::{Cli, CompletionsInstallArgs, SupportedShell};

pub mod install;

/// Writes a shell completion script for `shell` to stdout, generated from
/// the existing clap `Cli` definition. The `init` half of the install pair.
pub fn run_init(shell: SupportedShell) -> Result<()> {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    match shell {
        SupportedShell::Bash => generate(
            clap_complete::Shell::Bash,
            &mut cmd,
            name,
            &mut std::io::stdout(),
        ),
        SupportedShell::Zsh => generate(
            clap_complete::Shell::Zsh,
            &mut cmd,
            name,
            &mut std::io::stdout(),
        ),
        SupportedShell::Fish => generate(
            clap_complete::Shell::Fish,
            &mut cmd,
            name,
            &mut std::io::stdout(),
        ),
        SupportedShell::Powershell => generate(
            clap_complete::Shell::PowerShell,
            &mut cmd,
            name,
            &mut std::io::stdout(),
        ),
        SupportedShell::Elvish => generate(
            clap_complete::Shell::Elvish,
            &mut cmd,
            name,
            &mut std::io::stdout(),
        ),
        SupportedShell::Nushell => generate(
            clap_complete_nushell::Nushell,
            &mut cmd,
            name,
            &mut std::io::stdout(),
        ),
    }
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

/// Install completions into the user's shell config.
pub fn run_install(args: CompletionsInstallArgs) -> Result<()> {
    let outcome = install::run(args)?;
    print!("{}", install::render_outcome(&outcome));
    Ok(())
}
