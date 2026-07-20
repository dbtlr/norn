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
use crate::display::{emit, Diagnostic, Presenter};

mod cli;
mod commands;
/// The presentation seam. Public because the command ports and their tests
/// present Reports through it; the internal parse routing (`cli`, `commands`)
/// stays private.
pub mod display;
mod grammar_flags;
mod help;
mod output;
mod routed;

/// Crate-internal, test-only shared helpers (e.g. the panic-safe `EnvGuard`
/// every env-mutating test goes through). Compiled only under `cfg(test)`.
#[cfg(test)]
pub(crate) mod test_support;

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
    // Forgiving-input normalization (ADR 0010): desugar dynamic `--field value`
    // predicates and the alias pack on the query family BEFORE clap parses, using
    // a real clap-derived KnownFlags. A normalization error (a valueless dynamic
    // flag, an ambiguous repeat, a cross-family predicate on a mutate verb) is a
    // usage error (exit 2), reported before parse.
    let mut presenter = Presenter::stdio();
    let raw: Vec<String> = std::env::args().collect();
    let flags = grammar_flags::derive_known_flags();
    let (normalized, dynamic_fields) = match norn_core::grammar::normalize_argv(raw, &flags) {
        Ok(n) => (n.argv, n.dynamic_keys),
        Err(e) => {
            // A normalization error (valueless dynamic flag, ambiguous repeat,
            // cross-family predicate) is a usage error. Its message already
            // carries the did-you-mean the `closest` machinery computed; route it
            // through the single diagnostic path so the shape stays uniform.
            presenter.diagnostic(&e.to_string());
            return display::EXIT_USAGE;
        }
    };
    let mut cli = match Cli::try_parse_from(&normalized) {
        Ok(cli) => cli,
        Err(e) => {
            // clap prints its own usage/version output and picks the exit code
            // (0 for --version/help paths handled above, 2 for a bad invocation).
            e.print().ok();
            return e.exit_code();
        }
    };
    // The desugared dynamic-field keys are not a grammar flag — carry them from
    // normalization into the parsed command so the query verbs can forward them
    // to the owner-side field-universe gate (NRN-367).
    cli.global.dynamic_fields = dynamic_fields;
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
        // The ported read verbs resolve their report into an `Output` and the
        // single `emit` call renders it; the rest of the v0.48 surface is
        // grammar-only stubs (NRN-329) routed to the uniform not-yet-ported
        // outcome by name.
        Command::Find(args) => emit(
            commands::find::run(&args, &cli.global),
            &cli.global,
            presenter,
        ),
        Command::Get(args) => emit(
            commands::get::run(&args, &cli.global),
            &cli.global,
            presenter,
        ),
        Command::Count(args) => emit(
            commands::count::run(&args, &cli.global),
            &cli.global,
            presenter,
        ),
        Command::Describe(args) => emit(
            commands::describe::run(&args, &cli.global),
            &cli.global,
            presenter,
        ),
        Command::Set(_) => presenter.not_yet_ported("set"),
        Command::Edit(_) => presenter.not_yet_ported("edit"),
        Command::New(_) => presenter.not_yet_ported("new"),
        Command::Init(_) => presenter.not_yet_ported("init"),
        Command::Move(_) => presenter.not_yet_ported("move"),
        Command::Delete(_) => presenter.not_yet_ported("delete"),
        Command::Apply(_) => presenter.not_yet_ported("apply"),
        Command::Repair(_) => presenter.not_yet_ported("repair"),
        Command::RewriteWikilink(_) => presenter.not_yet_ported("rewrite-wikilink"),
        Command::Validate(args) => emit(
            commands::validate::run(&args, &cli.global),
            &cli.global,
            presenter,
        ),
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
        // and hand it the effective cwd, then render the returned Output through
        // the same single `emit` seam. The ambient-resolution failures stay
        // headline-only diagnostics (a bad config home / cwd has no useful
        // recovery hint), byte-identical to the donor.
        Command::Vault(cmd) => {
            let result = match ConfigHome::from_env() {
                Ok(home) => match effective_cwd(&cli.global) {
                    Ok(cwd) => commands::vault::run(&cmd, home, &cwd),
                    Err(msg) => Err(Diagnostic::new(msg)),
                },
                Err(err) => Err(Diagnostic::new(err.to_string())),
            };
            emit(result, &cli.global, presenter)
        }
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
    fn dispatch_set_presents_not_yet_ported() {
        let cli = Cli::try_parse_from(["norn", "set", "a.md", "status=done"]).unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut p = Presenter::new(&mut out, &mut err);
            dispatch(cli, &mut p)
        };
        assert_eq!(code, display::EXIT_OPERATIONAL);
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "norn: `set` is not yet ported in this build (rewrite in progress; see ADR 0018)\n"
        );
    }
}
