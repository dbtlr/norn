#![forbid(unsafe_code)]
//! Thin CLI adapter: parse argv to Params, present Reports. One command-module pattern, one display-helper layer.
//!
//! May never: Contain verb logic or business logic beyond how the CLI itself works.
//!
//! # Shape
//!
//! - [`cli`] — the clap root: global flags plus the `Command` enum, declarations only.
//! - [`commands`] — one module per command (clap `Args` + `to_params` + `run`). The
//!   read verbs (`find` / `count` / `get` / `describe` / `validate` / `repair`), the
//!   mutation verbs (`set` / `new` / `edit` / `move` / `delete` / `rewrite_wikilink`
//!   / `apply`), the `vault` registry namespace, and `mcp` (which resolves and
//!   summons a session like a read verb, then runs the MCP stdio server) execute
//!   for real; the still-unported surfaces (`init`, `completions`, `cache`, `config`,
//!   `self-update`, `serve`, `service`, `audit`, `manpage`) route to the uniform
//!   not-yet-ported outcome by name (NRN-329).
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
use crate::display::{emit, emit_mutation, Diagnostic, Presenter};

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
    norn_mcp::CONTRACT,
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
        // The seven mutation verbs that write (repair stays read-only; init
        // is not yet ported) share one interactive-confirm shape (NRN-389):
        // `emit_mutation` renders the first (ladder-derived)
        // report exactly like `emit` would, and — ONLY when `can_prompt` is
        // true and stdin turns out to be a real terminal and that first
        // render was a clean, unrefused forecast — prompts on stderr and, on
        // 'y', re-runs the SAME verb with `confirm: true` (a second routed
        // request, re-planned fresh under the owner's lock) and renders that
        // as the final outcome. `can_prompt` mirrors the shared
        // `--dry-run`/`--yes`/`--format json` mode ladder each verb's `run`
        // already resolves `confirm` from: an explicit `--dry-run` or
        // `--yes` already answered the apply question, and `--format json`
        // is output-shape-only and never implies interactive consent — none
        // of those three should ever reach a prompt.
        Command::Set(args) => {
            let can_prompt = args.mode.can_prompt();
            emit_mutation(
                commands::set::run(&args, &cli.global),
                can_prompt,
                || commands::set::run_confirm(&args, &cli.global, true),
                &cli.global,
                presenter,
            )
        }
        Command::Edit(args) => {
            let can_prompt = args.mode.can_prompt();
            emit_mutation(
                commands::edit::run(&args, &cli.global),
                can_prompt,
                || commands::edit::run_confirm(&args, &cli.global, true),
                &cli.global,
                presenter,
            )
        }
        Command::New(args) => {
            let can_prompt = args.mode.can_prompt();
            emit_mutation(
                commands::new::run(&args, &cli.global),
                can_prompt,
                || commands::new::run_confirm(&args, &cli.global, true),
                &cli.global,
                presenter,
            )
        }
        Command::Init(_) => presenter.not_yet_ported("init"),
        Command::Move(args) => {
            let can_prompt = args.mode.can_prompt();
            emit_mutation(
                commands::move_doc::run(&args, &cli.global),
                can_prompt,
                || commands::move_doc::run_confirm(&args, &cli.global, true),
                &cli.global,
                presenter,
            )
        }
        Command::Delete(args) => {
            let can_prompt = args.mode.can_prompt();
            emit_mutation(
                commands::delete::run(&args, &cli.global),
                can_prompt,
                || commands::delete::run_confirm(&args, &cli.global, true),
                &cli.global,
                presenter,
            )
        }
        Command::Apply(args) => {
            let can_prompt = args.mode.can_prompt();
            emit_mutation(
                commands::apply::run(&args, &cli.global),
                can_prompt,
                || commands::apply::run_confirm(&args, &cli.global, true),
                &cli.global,
                presenter,
            )
        }
        Command::Repair(args) => emit(
            commands::repair::run(&args, &cli.global),
            &cli.global,
            presenter,
        ),
        Command::RewriteWikilink(args) => {
            let can_prompt = args.mode.can_prompt();
            emit_mutation(
                commands::rewrite_wikilink::run(&args, &cli.global),
                can_prompt,
                || commands::rewrite_wikilink::run_confirm(&args, &cli.global, true),
                &cli.global,
                presenter,
            )
        }
        Command::Validate(args) => emit(
            commands::validate::run(&args, &cli.global),
            &cli.global,
            presenter,
        ),
        Command::Completions(_) => presenter.not_yet_ported("completions"),
        Command::Cache(_) => presenter.not_yet_ported("cache"),
        Command::Config(_) => presenter.not_yet_ported("config"),
        Command::SelfUpdate(_) => presenter.not_yet_ported("self-update"),
        // `norn mcp` speaks the MCP stdio protocol. The CLI owns vault resolution
        // + summon (exactly like a read verb), then hands the ready session to the
        // MCP adapter, which runs the JSON-RPC server until the client closes
        // stdin. A resolution failure is a soft-landing diagnostic; a serve-loop
        // failure is an operational diagnostic.
        Command::Mcp(_) => match routed::open_session(&cli.global) {
            Ok(session) => match norn_mcp::serve_stdio(session) {
                Ok(()) => display::EXIT_OK,
                Err(err) => {
                    presenter.diagnostic(&err.to_string());
                    display::EXIT_OPERATIONAL
                }
            },
            Err(diag) => {
                presenter.present_diagnostic(&diag);
                display::EXIT_OPERATIONAL
            }
        },
        Command::Serve(_) => presenter.not_yet_ported("serve"),
        Command::Service(_) => presenter.not_yet_ported("service"),
        Command::Audit(_) => presenter.not_yet_ported("audit"),
        Command::Manpage => presenter.not_yet_ported("manpage"),
        // The registry namespace (ADR 0017), the first namespace that EXECUTES —
        // resolve the ambient config home and hand it the effective cwd, then
        // render the returned Output through the same single `emit` seam. The
        // ambient-resolution failures stay headline-only diagnostics (a bad
        // config home / cwd has no useful recovery hint).
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
    fn dispatch_edit_cli_side_refusal_renders_error_and_exits_two() {
        // `edit` now dispatches for real. A CLI-side op-resolution failure (an
        // empty ops array) is refused BEFORE any owner summon — so this exercises
        // the dispatch → render_edit refusal path without touching the network:
        // `error: <message>` on stderr, exit 2, for both formats.
        let cli = Cli::try_parse_from(["norn", "edit", "a.md", "--edits-json", "[]"]).unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut p = Presenter::new(&mut out, &mut err);
            dispatch(cli, &mut p)
        };
        assert_eq!(code, display::EXIT_USAGE);
        assert!(out.is_empty(), "a refusal writes nothing to stdout");
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "error: edits array is empty\n"
        );
    }
}
