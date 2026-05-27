//! Custom help renderer per the CLI Help Output v2 spec.
//!
//! This module owns rendering for both `-h` and `--help`. clap is used as the
//! argument parser and the source of arg metadata; it does not emit help text.

pub mod bin_name;
pub mod examples;
pub mod extract;
pub mod find_live;
pub mod model;
pub mod render;

pub use bin_name::BIN_NAME;
pub use extract::build_model;
pub use model::HelpForm;

use std::io::{self, IsTerminal, Write};

use clap::CommandFactory;

use crate::cli::Cli;
use crate::output::pager::{should_page, spawn_pager_or_passthrough};
use crate::output::palette;

/// Called from `main()` BEFORE `Cli::parse()`. Scans `std::env::args()` for
/// `-h` / `--help`, resolves the subcommand path from the raw args, and renders
/// help. Returns `Some(exit_code)` when help was rendered; `None` otherwise.
///
/// This pre-parse approach is necessary because required positionals (e.g.
/// `vault completions init --help`) would cause `Cli::parse()` to error out
/// before we get a chance to intercept. By scanning raw args first we can
/// render help without satisfying required positionals.
pub fn intercept_from_args() -> Option<i32> {
    let args: Vec<String> = std::env::args().collect();

    // Determine form: long (--help) takes priority over short (-h).
    let form = if args.iter().any(|a| a == "--help") {
        HelpForm::Long
    } else if args.iter().any(|a| a == "-h") {
        HelpForm::Short
    } else {
        return None;
    };

    // Resolve the color setting from raw args (default auto).
    let color = parse_color_from_args(&args);
    let palette = palette::resolve(color);
    let term_width = terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80);

    let mut root = Cli::command();
    if !crate::self_update::receipt::exists() {
        root = root.mut_subcommand("self-update", |sc| sc.hide(true));
    }
    let (subcmd, cmd_path, hit_unknown) = resolve_subcmd_from_raw_args(&root, &args);
    if hit_unknown {
        // An unknown token appeared before the help flag. Let Cli::parse()
        // run so clap can report the "unrecognized subcommand" error.
        return None;
    }
    let mut model = build_model(subcmd, &root, &cmd_path, form);

    // Phase 3 — materialize live examples on `--help` form only. Gate on the
    // command having a generator AND the effective cwd being a vault root
    // (`.vault/` present). If `Cache::open` fails for any reason, fall back
    // silently to the no-live-examples path — help must never error.
    if form == HelpForm::Long {
        if let Some(generator) = model.extras.live_examples_fn {
            let cwd_arg = parse_cwd_from_args(&args);
            if let Ok(root_path) = crate::config_loader::effective_cwd(cwd_arg.as_ref()) {
                if root_path.join(".vault").as_std_path().is_dir() {
                    if let Ok(cache) = crate::cache::Cache::open(&root_path) {
                        model.live_examples = generator(&cache);
                    }
                }
            }
        }
    }

    let mut buf: Vec<u8> = Vec::new();
    let render_result = match form {
        HelpForm::Short => render::render_short(&mut buf, &model, &palette, term_width),
        HelpForm::Long => render::render_long(&mut buf, &model, &palette, term_width),
    };
    if let Err(err) = render_result {
        eprintln!("{BIN_NAME}: help render failed: {err}");
        return Some(1);
    }

    let stdout = io::stdout();
    let is_tty = stdout.is_terminal();
    let result = match form {
        HelpForm::Long => {
            let buffer_lines = buf.iter().filter(|b| **b == b'\n').count();
            if should_page(buffer_lines, /* no_pager */ false, is_tty) {
                let mut stderr = io::stderr();
                let mut out = stdout.lock();
                spawn_pager_or_passthrough(&buf, &mut out, &mut stderr, "vault --help")
            } else {
                io::stdout().write_all(&buf)
            }
        }
        HelpForm::Short => io::stdout().write_all(&buf),
    };
    if let Err(err) = result {
        if err.kind() != io::ErrorKind::BrokenPipe {
            eprintln!("{BIN_NAME}: writing help failed: {err}");
            return Some(1);
        }
    }
    Some(0)
}

/// Walk the raw args to find the deepest recognised subcommand chain, then
/// return the matching `clap::Command`, the user-facing path string, and a
/// flag indicating whether an unknown non-flag token was encountered (i.e. a
/// token that looks like a subcommand but is not recognised).
///
/// `hit_unknown = true` means the args contain something like `vault graph
/// --help` where `graph` is not a known subcommand. In that case the caller
/// should NOT intercept, so that clap can produce its normal error.
///
/// Strategy: skip the binary name (args[0]) and any flag-like tokens (`-…`
/// / `--…`). Walk non-flag tokens as subcommand names, diving into the clap
/// tree as long as each token is a valid subcommand name. Stop at the first
/// token that is not a known subcommand.
fn resolve_subcmd_from_raw_args<'a>(
    root: &'a clap::Command,
    args: &[String],
) -> (&'a clap::Command, String, bool) {
    let mut current = root;
    let mut path = BIN_NAME.to_string();

    // Skip args[0] (binary path) and walk subcommand tokens.
    let mut iter = args.iter().skip(1);
    while let Some(token) = iter.next() {
        // Skip flags and their inline values (--foo=val or --foo val).
        if token.starts_with('-') {
            // If it's a `--key=value` form, nothing extra to skip.
            // If it's a `--key value` form, skip the next token as the value.
            if !token.contains('=') {
                // Known value-taking global flags: --cwd (-C) and --config.
                // We skip their next token to avoid mistaking it for a subcmd.
                let flag_stem = token.trim_start_matches('-');
                if matches!(flag_stem, "cwd" | "C" | "config" | "color") {
                    let _ = iter.next(); // skip value
                }
            }
            continue;
        }
        // Try to descend into this token as a subcommand name.
        if let Some(child) = current
            .get_subcommands()
            .find(|c| c.get_name() == token.as_str())
        {
            path = format!("{path} {token}");
            current = child;
        } else {
            // Token is not a known subcommand. It might be a positional value
            // (valid) or an unknown subcommand name (error). We flag it as
            // unknown when we're still at the root or an intermediate subcommand
            // level that accepts subcommands — the caller then passes through to
            // clap for proper error reporting.
            let expecting_subcommand = current.has_subcommands();
            return (current, path, expecting_subcommand);
        }
    }

    (current, path, false)
}

/// Parse `--cwd <PATH>` (or `-C <PATH>`, or `--cwd=PATH`) from raw args.
/// Returns `None` when the flag is absent or the value is not UTF-8.
fn parse_cwd_from_args(args: &[String]) -> Option<camino::Utf8PathBuf> {
    let mut iter = args.iter();
    while let Some(token) = iter.next() {
        if token == "--cwd" || token == "-C" {
            if let Some(val) = iter.next() {
                return camino::Utf8PathBuf::from_path_buf(std::path::PathBuf::from(val)).ok();
            }
        } else if let Some(val) = token.strip_prefix("--cwd=") {
            return camino::Utf8PathBuf::from_path_buf(std::path::PathBuf::from(val)).ok();
        }
    }
    None
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
