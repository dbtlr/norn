//! Custom help renderer per the CLI Help Output v2 spec.
//!
//! This module owns rendering for both `-h` and `--help`. clap is the argument
//! parser and the source of arg metadata; it does not emit help text (the root
//! and every subcommand set `disable_help_flag` + `disable_help_subcommand`,
//! and the `-h` / `--help` globals are plain bool flags this interceptor reads).
//!
//! `--help` (the long form only — `-h` never pages) spawns the same TTY pager
//! `find` / `get` do (NRN-454): a long render on a real terminal pages through
//! `$PAGER`; piped/non-TTY output and a short render write straight to stdout,
//! unchanged from before the pager existed.
//!
//! Deliberate simplifications:
//! - No live-examples materialization: there is no query core yet, and `--help`
//!   opens an empty cache and would emit none anyway (see `model.rs`).
//! - `self-update` is always listed, unconditionally.

pub mod bin_name;
pub mod examples;
pub mod extract;
pub mod model;
pub mod plan_example;
pub mod render;

pub use bin_name::BIN_NAME;
pub use extract::build_model;
pub use model::HelpForm;

use std::io::{self, IsTerminal, Write};

use clap::CommandFactory;

use crate::cli::Cli;
use crate::display::{Conversation, Presenter};
use crate::output::pager;
use crate::output::palette;

/// Fixed wrap width. Wrapping affects only the short-form description line and
/// never the flag lines or any compared help output; a piped (non-TTY) run has
/// no terminal width, so a stable 80 keeps output deterministic.
const TERM_WIDTH: usize = 80;

/// Scan `std::env::args()` for `-h` / `--help`, resolve the subcommand path
/// from the raw args, and render help. Returns `Some(exit_code)` when help was
/// rendered; `None` otherwise. Called from `run()` BEFORE `Cli::parse()`.
///
/// Pre-parse is necessary because required positionals (e.g. `norn move --help`)
/// would make `Cli::parse()` error before we could intercept.
pub fn intercept_from_args() -> Option<i32> {
    let args: Vec<String> = std::env::args().collect();
    render_help_for_args(&args)
}

/// Render a subcommand's long-form help into a byte buffer — used by the read
/// verbs' no-argument help gate (bare `norn find` prints its help and exits
/// 2). Unknown command names fall back to the root help.
pub fn render_command_long(command_name: &str, color: crate::cli::ColorWhen) -> Vec<u8> {
    let root = Cli::command();
    let subcmd = root
        .get_subcommands()
        .find(|c| c.get_name() == command_name)
        .unwrap_or(&root);
    let path = format!("{BIN_NAME} {command_name}");
    let model = build_model(subcmd, &root, &path, HelpForm::Long);
    let palette = palette::resolve(color);
    let mut buf: Vec<u8> = Vec::new();
    let _ = render::render_long(&mut buf, &model, &palette, TERM_WIDTH);
    buf
}

/// The testable core of [`intercept_from_args`]: given the raw argv, render
/// help to real stdout and return `Some(exit)`, or `None` when no help flag is
/// present / an unknown subcommand should fall through to clap's error.
fn render_help_for_args(args: &[String]) -> Option<i32> {
    // Determine form: long (--help) takes priority over short (-h).
    let form = if args.iter().any(|a| a == "--help") {
        HelpForm::Long
    } else if args.iter().any(|a| a == "-h") {
        HelpForm::Short
    } else {
        return None;
    };

    let color = parse_color_from_args(args);
    let palette = palette::resolve(color);

    let root = Cli::command();
    let (subcmd, cmd_path, hit_unknown) = resolve_subcmd_from_raw_args(&root, args);
    if hit_unknown {
        // An unknown token appeared before the help flag. Let `Cli::parse()`
        // run so clap can report the "unrecognized subcommand" error.
        return None;
    }
    let model = build_model(subcmd, &root, &cmd_path, form);

    let mut buf: Vec<u8> = Vec::new();
    let render_result = match form {
        HelpForm::Short => render::render_short(&mut buf, &model, &palette, TERM_WIDTH),
        HelpForm::Long => render::render_long(&mut buf, &model, &palette, TERM_WIDTH),
    };
    if let Err(err) = render_result {
        // Route the failure through the single stderr diagnostic path (the
        // `norn:` headline convention), rather than a raw stderr write — the
        // pre-parse help interceptor runs before dispatch, so it builds its own
        // stdio presenter. Emits the same `{BIN_NAME}: …` line the standard
        // diagnostic path produces.
        Presenter::stdio().diagnostic(&format!("help render failed: {err}"));
        return Some(1);
    }

    // Long-form help alone is pageable — `-h`'s short form is a one-line-per-flag
    // summary, never long enough to page. There is no `--help`-local
    // `--no-pager` flag (this interceptor runs before clap parses anything),
    // so the suppress input is always `false`.
    let is_tty = io::stdout().is_terminal();
    let buffer_lines = pager::count_lines(&buf);
    let should_page = form == HelpForm::Long
        && pager::should_page(buffer_lines, /* no_pager */ false, is_tty);

    let write_result: io::Result<()> = if should_page {
        let mut stderr = io::stderr();
        let mut conv = Conversation::new(&mut stderr);
        let mut stdout = io::stdout();
        pager::page_or_write_direct(&buf, &mut stdout, &mut conv)
    } else {
        io::stdout().write_all(&buf)
    };

    if let Err(err) = write_result {
        if err.kind() != io::ErrorKind::BrokenPipe {
            Presenter::stdio().diagnostic(&format!("writing help failed: {err}"));
            return Some(1);
        }
    }
    Some(0)
}

/// Walk the raw args to find the deepest recognised subcommand chain, then
/// return the matching `clap::Command`, the user-facing path string, and a flag
/// indicating whether an unknown non-flag token was encountered.
///
/// `hit_unknown = true` means the args contain something like `norn graph
/// --help` where `graph` is not a known subcommand — the caller then declines
/// to intercept so clap can produce its normal error.
fn resolve_subcmd_from_raw_args<'a>(
    root: &'a clap::Command,
    args: &[String],
) -> (&'a clap::Command, String, bool) {
    let mut current = root;
    let mut path = BIN_NAME.to_string();

    let mut iter = args.iter().skip(1);
    while let Some(token) = iter.next() {
        if token.starts_with('-') {
            // Skip flags and their inline values (`--foo=val` or `--foo val`).
            if !token.contains('=') {
                let flag_stem = token.trim_start_matches('-');
                if matches!(flag_stem, "cwd" | "C" | "color" | "vault") {
                    let _ = iter.next(); // skip the value
                }
            }
            continue;
        }
        if let Some(child) = current
            .get_subcommands()
            .find(|c| c.get_name() == token.as_str())
        {
            path = format!("{path} {token}");
            current = child;
        } else {
            // Not a known subcommand: a positional value (valid) or an unknown
            // subcommand name (error). Flag it as unknown when this level
            // accepts subcommands, so the caller passes through to clap.
            let expecting_subcommand = current.has_subcommands();
            return (current, path, expecting_subcommand);
        }
    }

    (current, path, false)
}

/// Parse `--color <VALUE>` from raw args, defaulting to `ColorWhen::Auto`.
fn parse_color_from_args(args: &[String]) -> crate::cli::ColorWhen {
    use crate::cli::ColorWhen;
    let mut iter = args.iter();
    while let Some(token) = iter.next() {
        if token == "--color" {
            if let Some(val) = iter.next() {
                return match val.as_str() {
                    "always" => ColorWhen::Always,
                    "never" => ColorWhen::Never,
                    _ => ColorWhen::Auto,
                };
            }
        } else if let Some(val) = token.strip_prefix("--color=") {
            return match val {
                "always" => ColorWhen::Always,
                "never" => ColorWhen::Never,
                _ => ColorWhen::Auto,
            };
        }
    }
    ColorWhen::Auto
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn resolves_leaf_subcommand_path() {
        let root = Cli::command();
        let (_cmd, path, unknown) =
            resolve_subcmd_from_raw_args(&root, &argv(&["norn", "find", "--help"]));
        assert_eq!(path, "norn find");
        assert!(!unknown);
    }

    #[test]
    fn resolves_nested_subcommand_path() {
        let root = Cli::command();
        let (_cmd, path, unknown) =
            resolve_subcmd_from_raw_args(&root, &argv(&["norn", "cache", "index", "--help"]));
        assert_eq!(path, "norn cache index");
        assert!(!unknown);
    }

    #[test]
    fn bare_root_path_is_bin_name() {
        let root = Cli::command();
        let (_cmd, path, unknown) = resolve_subcmd_from_raw_args(&root, &argv(&["norn", "--help"]));
        assert_eq!(path, "norn");
        assert!(!unknown);
    }

    #[test]
    fn unknown_subcommand_flags_passthrough() {
        let root = Cli::command();
        let (_cmd, _path, unknown) =
            resolve_subcmd_from_raw_args(&root, &argv(&["norn", "nope", "--help"]));
        assert!(
            unknown,
            "an unknown top-level token must fall through to clap"
        );
    }

    #[test]
    fn cwd_value_is_not_mistaken_for_a_subcommand() {
        let root = Cli::command();
        // `find` is the subcommand; `/some/dir` is the -C value, not a token.
        let (_cmd, path, unknown) = resolve_subcmd_from_raw_args(
            &root,
            &argv(&["norn", "-C", "find", "validate", "--help"]),
        );
        // -C consumes `find`; `validate` is then the resolved subcommand.
        assert_eq!(path, "norn validate");
        assert!(!unknown);
    }

    #[test]
    fn vault_value_is_not_mistaken_for_a_subcommand() {
        let root = Cli::command();
        // `--vault atlas` — `atlas` is the flag's value, not a subcommand.
        let (_cmd, path, unknown) = resolve_subcmd_from_raw_args(
            &root,
            &argv(&["norn", "--vault", "atlas", "find", "--help"]),
        );
        assert_eq!(path, "norn find");
        assert!(!unknown);
        // Inline form never consumed a following token to begin with.
        let (_cmd, path, unknown) = resolve_subcmd_from_raw_args(
            &root,
            &argv(&["norn", "--vault=atlas", "find", "--help"]),
        );
        assert_eq!(path, "norn find");
        assert!(!unknown);
    }

    #[test]
    fn color_flag_parsed_from_args() {
        use crate::cli::ColorWhen;
        assert!(matches!(
            parse_color_from_args(&argv(&["norn", "--color", "never", "find"])),
            ColorWhen::Never
        ));
        assert!(matches!(
            parse_color_from_args(&argv(&["norn", "--color=always", "find"])),
            ColorWhen::Always
        ));
        assert!(matches!(
            parse_color_from_args(&argv(&["norn", "find"])),
            ColorWhen::Auto
        ));
    }

    #[test]
    fn no_help_flag_returns_none() {
        assert_eq!(
            render_help_for_args(&argv(&["norn", "find", "--all"])),
            None
        );
    }
}
