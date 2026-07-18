#![forbid(unsafe_code)]
//! Thin CLI adapter: parse argv to Params, present Reports. One command-module pattern, one display-helper layer.
//!
//! May never: Contain verb logic or business logic beyond how the CLI itself works.
//!
//! # Shape
//!
//! - [`cli`] — the clap root: global flags plus the `Command` enum, declarations only.
//! - [`commands`] — one module per command (clap `Args` + `to_params` + `run`); two
//!   exemplars this phase (`find`, `get`), the rest fill in as the verbs port (NRN-329).
//! - [`display`] — the presentation layer: the output-format vocabulary, the stderr
//!   `norn:` convention, and the uniform not-yet-ported outcome.
//!
//! [`run`] is the one entry the bin calls: clap parses (handling `--help` /
//! `--version` at exit 0 and bad invocation at exit 2 itself), then a single
//! [`dispatch`] match hands the parsed command to its module.

use std::io::Write;

use clap::Parser;
use norn_config::ConfigHome;

use crate::cli::{Cli, Command};
use crate::display::Presenter;

mod cli;
mod commands;
/// The presentation seam. Public because the command ports and their tests
/// present Reports through it; the internal parse routing (`cli`, `commands`)
/// stays private.
pub mod display;
mod help;
mod output;

/// One-line boundary contract, referenced by every dependent so each
/// declared edge in the crate map is compiler-load-bearing.
pub const CONTRACT: &str = "norn-cli: thin CLI adapter — parse and present only";

/// Direct-dependency contracts — the code reference that makes this
/// crate's declared edges load-bearing rather than manifest-only.
pub const DEP_CONTRACTS: &[&str] = &[
    norn_client::CONTRACT,
    norn_config::CONTRACT,
    norn_wire::CONTRACT,
];

/// Parse the process argv and run to a process exit code.
///
/// The custom help renderer (`crate::help`) intercepts `-h` / `--help` BEFORE
/// `Cli::parse()` — the root and every subcommand disable clap's own help, so
/// help must be rendered from the derive tree by walking it. A rendered help
/// returns exit 0 here; otherwise clap parses (`--version` at exit 0, bad
/// invocation at exit 2, calling `process::exit` itself) and a successful parse
/// dispatches to the command module, whose returned code becomes the process
/// exit code.
pub fn run() -> i32 {
    if let Some(code) = help::intercept_from_args() {
        return code;
    }
    let cli = Cli::parse();
    let mut presenter = Presenter::stdio();
    dispatch(cli, &mut presenter)
}

/// The one dispatch match: parsed command → its module's `run`.
///
/// The `vault` namespace executes now (unlike the read-verb stubs), so its
/// dispatch arm resolves the ambient [`ConfigHome`] here — the single place the
/// process environment is read for central config — and injects it into the
/// command run. A config-home that cannot be determined (e.g. a relative
/// `NORN_CONFIG_DIR`) fails loud as an operational diagnostic.
fn dispatch<O: Write, E: Write>(cli: Cli, presenter: &mut Presenter<O, E>) -> i32 {
    match cli.command {
        // The two ported read exemplars parse to Params and present the uniform
        // not-yet-ported outcome; the rest of the v0.48 surface is grammar-only
        // stubs (NRN-329) routed to the same uniform outcome by name.
        Command::Find(args) => commands::find::run(&args, presenter),
        Command::Get(args) => commands::get::run(&args, presenter),
        Command::Count(_) => presenter.not_yet_ported("count"),
        Command::Describe(_) => presenter.not_yet_ported("describe"),
        Command::Set(_) => presenter.not_yet_ported("set"),
        Command::Edit(_) => presenter.not_yet_ported("edit"),
        Command::New(_) => presenter.not_yet_ported("new"),
        Command::Init(_) => presenter.not_yet_ported("init"),
        Command::Move(_) => presenter.not_yet_ported("move"),
        Command::Delete(_) => presenter.not_yet_ported("delete"),
        Command::Apply(_) => presenter.not_yet_ported("apply"),
        Command::Repair(_) => presenter.not_yet_ported("repair"),
        Command::RewriteWikilink(_) => presenter.not_yet_ported("rewrite-wikilink"),
        Command::Validate(_) => presenter.not_yet_ported("validate"),
        Command::Completions(_) => presenter.not_yet_ported("completions"),
        Command::Cache(_) => presenter.not_yet_ported("cache"),
        Command::Config(_) => presenter.not_yet_ported("config"),
        Command::SelfUpdate(_) => presenter.not_yet_ported("self-update"),
        Command::Mcp(_) => presenter.not_yet_ported("mcp"),
        Command::Serve(_) => presenter.not_yet_ported("serve"),
        Command::Service(_) => presenter.not_yet_ported("service"),
        Command::Audit(_) => presenter.not_yet_ported("audit"),
        Command::Manpage => presenter.not_yet_ported("manpage"),
        // The intentionally-new registry namespace (ADR 0017): no oracle, and
        // the first namespace that EXECUTES — resolve the ambient config home
        // and hand it the effective cwd.
        Command::Vault(cmd) => match ConfigHome::from_env() {
            Ok(home) => match effective_cwd(&cli.global) {
                Ok(cwd) => commands::vault::run(&cmd, home, &cwd, presenter),
                Err(msg) => {
                    presenter.diagnostic(&msg);
                    display::EXIT_OPERATIONAL
                }
            },
            Err(err) => {
                presenter.diagnostic(&err.to_string());
                display::EXIT_OPERATIONAL
            }
        },
    }
}

/// The directory vault path arguments resolve against: `-C/--cwd` when given
/// (grounded against the process cwd if itself relative), else the process
/// cwd. This is the only place the process cwd is read.
fn effective_cwd(global: &cli::GlobalArgs) -> Result<std::path::PathBuf, String> {
    let ground = |dir: Option<&std::path::Path>| {
        std::env::current_dir()
            .map(|cwd| dir.map_or_else(|| cwd.clone(), |d| cwd.join(d)))
            .map_err(|source| format!("cannot read the current directory: {source}"))
    };
    match &global.cwd {
        Some(dir) if dir.is_absolute() => Ok(dir.clone()),
        Some(dir) => ground(Some(dir)),
        None => ground(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_find_presents_not_yet_ported() {
        let cli = Cli::try_parse_from(["norn", "find", "--all"]).unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut p = Presenter::new(&mut out, &mut err);
            dispatch(cli, &mut p)
        };
        assert_eq!(code, display::EXIT_OPERATIONAL);
        assert!(out.is_empty());
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "norn: `find` is not yet ported in this build (rewrite in progress; see ADR 0018)\n"
        );
    }

    #[test]
    fn dispatch_get_presents_not_yet_ported() {
        let cli = Cli::try_parse_from(["norn", "get", "alpha"]).unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut p = Presenter::new(&mut out, &mut err);
            dispatch(cli, &mut p)
        };
        assert_eq!(code, display::EXIT_OPERATIONAL);
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "norn: `get` is not yet ported in this build (rewrite in progress; see ADR 0018)\n"
        );
    }
}
