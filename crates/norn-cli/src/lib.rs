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

use crate::cli::{Cli, Command};
use crate::display::Presenter;

mod cli;
mod commands;
/// The presentation seam. Public because the command ports and their tests
/// present Reports through it; the internal parse routing (`cli`, `commands`)
/// stays private.
pub mod display;

/// One-line boundary contract, referenced by every dependent so each
/// declared edge in the crate map is compiler-load-bearing.
pub const CONTRACT: &str = "norn-cli: thin CLI adapter — parse and present only";

/// Direct-dependency contracts — the code reference that makes this
/// crate's declared edges load-bearing rather than manifest-only.
pub const DEP_CONTRACTS: &[&str] = &[norn_client::CONTRACT, norn_wire::CONTRACT];

/// Parse the process argv and run to a process exit code.
///
/// clap handles `--help` / `--version` (exit 0) and bad invocation — unknown
/// command, bad flag, missing required arg — (exit 2) by itself, calling
/// `process::exit` before returning. A successful parse dispatches to the
/// command module, whose returned code becomes the process exit code.
pub fn run() -> i32 {
    let cli = Cli::parse();
    let mut presenter = Presenter::stdio();
    dispatch(cli, &mut presenter)
}

/// The one dispatch match: parsed command → its module's `run`.
fn dispatch<O: Write, E: Write>(cli: Cli, presenter: &mut Presenter<O, E>) -> i32 {
    match cli.command {
        Command::Find(args) => commands::find::run(&args, presenter),
        Command::Get(args) => commands::get::run(&args, presenter),
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
