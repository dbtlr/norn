//! The clap command surface for the `norn` binary: the `Cli` parser, the
//! global flags, and the `Command` enum with one variant per verb.
//!
//! Declarations only — no command logic. `crate::dispatch` matches on the
//! `Command` this produces and hands each variant to its command module.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::commands::{find::FindArgs, get::GetArgs, vault::VaultCmd};

#[derive(Debug, Parser)]
#[command(name = "norn")]
#[command(about = "Deterministic Markdown vault graph tools")]
#[command(version)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,

    #[command(subcommand)]
    pub command: Command,
}

/// Flags accepted before or after the subcommand (`global = true`). Parsed this
/// phase but not yet wired to behavior — vault resolution lands with the
/// summoner (`norn-client`), which consumes these to pick the target vault.
#[derive(Debug, Args)]
pub struct GlobalArgs {
    /// Run as if norn started in this directory (default: the current
    /// directory).
    #[arg(
        short = 'C',
        long,
        global = true,
        value_name = "DIR",
        help_heading = "Global options"
    )]
    pub cwd: Option<PathBuf>,

    /// Vault config file. Defaults to the resolved vault's config when present.
    #[arg(
        long,
        global = true,
        value_name = "FILE",
        help_heading = "Global options"
    )]
    pub config: Option<PathBuf>,

    /// Target the registered vault with this name (ADR 0017).
    #[arg(
        long,
        global = true,
        value_name = "NAME",
        help_heading = "Global options"
    )]
    pub vault: Option<String>,
}

// clap's `Subcommand` derive requires each variant's payload to impl `Args`,
// which `Box<T>` does not — so the lint's boxing fix is unavailable here. The
// size asymmetry between the filter-heavy `find` and the small `get` is
// inherent to the surface.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(
        about = "Find documents in the vault — full-text + metadata filters with sort/limit/paging"
    )]
    Find(FindArgs),
    #[command(
        about = "Get one or more documents — frontmatter, headings, outgoing/incoming/unresolved links"
    )]
    Get(GetArgs),
    #[command(
        subcommand,
        about = "Manage the vault registry — register a vault to unlock durable artifacts (cache, event stream, logs)"
    )]
    Vault(VaultCmd),
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn global_cwd_accepted_before_subcommand() {
        let cli = Cli::try_parse_from(["norn", "-C", "/x", "find", "--all"]).unwrap();
        assert_eq!(cli.global.cwd.as_deref(), Some(std::path::Path::new("/x")));
    }

    #[test]
    fn global_cwd_accepted_after_subcommand() {
        let cli = Cli::try_parse_from(["norn", "find", "--all", "-C", "/x"]).unwrap();
        assert_eq!(cli.global.cwd.as_deref(), Some(std::path::Path::new("/x")));
    }

    #[test]
    fn vault_name_flag_parses() {
        let cli = Cli::try_parse_from(["norn", "--vault", "atlas", "find", "--all"]).unwrap();
        assert_eq!(cli.global.vault.as_deref(), Some("atlas"));
    }

    #[test]
    fn config_flag_parses_after_subcommand() {
        let cli = Cli::try_parse_from(["norn", "get", "alpha", "--config", "/c.yaml"]).unwrap();
        assert_eq!(
            cli.global.config.as_deref(),
            Some(std::path::Path::new("/c.yaml"))
        );
    }

    #[test]
    fn unknown_command_is_a_parse_error() {
        assert!(Cli::try_parse_from(["norn", "nope"]).is_err());
    }

    #[test]
    fn derive_tree_is_valid() {
        // Catches derive-level ambiguities (duplicate flags, bad global setup)
        // at test time rather than first real invocation.
        Cli::command().debug_assert();
    }
}
