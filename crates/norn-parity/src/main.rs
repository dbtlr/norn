#![forbid(unsafe_code)]
//! `norn-parity [--self-check | --all | --consistency] --oracle <path>
//! [--rewrite <path>] [--ledger <path>] [--suite <name>]...`
//!
//! Hand-parsed argv (std::env::args) — no `clap`, matching `norn-fixtures`'
//! bin (see `crates/norn-fixtures/src/main.rs`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use norn_parity::run::{self, Mode, RunConfig};
use norn_parity::{consistency, report};

fn usage() -> String {
    "usage: norn-parity [--self-check | --all | --consistency] --oracle <path> [--rewrite <path>] [--ledger <path>] [--suite <name>]...\n\n\
     modes (default: gated — ported==true suites only, oracle vs. rewrite):\n\
     \x20 --self-check   every case, oracle vs. itself; proves the cases + comparator are sound\n\
     \x20 --all          every suite regardless of ported, oracle vs. rewrite (burn-down view)\n\
     \x20 --consistency  oracle-only cross-command invariants; --rewrite not required\n\n\
     --oracle default: `norn` resolved from PATH\n\
     --rewrite default: ./target/release/norn (ignored by --consistency)\n\
     --ledger default: docs/parity-ledger.toml at the workspace root"
        .to_string()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModeFlag {
    Gated,
    SelfCheck,
    All,
    Consistency,
}

struct Args {
    mode: ModeFlag,
    oracle: String,
    rewrite: String,
    ledger: Option<PathBuf>,
    suite_filter: Vec<String>,
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut mode: Option<ModeFlag> = None;
    let mut oracle: Option<String> = None;
    let mut rewrite: Option<String> = None;
    let mut ledger: Option<PathBuf> = None;
    let mut suite_filter: Vec<String> = Vec::new();

    let set_mode = |mode: &mut Option<ModeFlag>, new: ModeFlag| -> Result<(), String> {
        match mode {
            Some(existing) if *existing != new => {
                Err("--self-check, --all, and --consistency are mutually exclusive".to_string())
            }
            _ => {
                *mode = Some(new);
                Ok(())
            }
        }
    };

    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--self-check" => set_mode(&mut mode, ModeFlag::SelfCheck)?,
            "--all" => set_mode(&mut mode, ModeFlag::All)?,
            "--consistency" => set_mode(&mut mode, ModeFlag::Consistency)?,
            "--oracle" => {
                i += 1;
                oracle = Some(argv.get(i).ok_or("--oracle requires a value")?.clone());
            }
            "--rewrite" => {
                i += 1;
                rewrite = Some(argv.get(i).ok_or("--rewrite requires a value")?.clone());
            }
            "--ledger" => {
                i += 1;
                ledger = Some(PathBuf::from(
                    argv.get(i).ok_or("--ledger requires a value")?,
                ));
            }
            "--suite" => {
                i += 1;
                suite_filter.push(argv.get(i).ok_or("--suite requires a value")?.clone());
            }
            other => return Err(format!("unexpected argument: {other}")),
        }
        i += 1;
    }

    Ok(Args {
        mode: mode.unwrap_or(ModeFlag::Gated),
        oracle: oracle.unwrap_or_else(|| "norn".to_string()),
        rewrite: rewrite.unwrap_or_else(|| "./target/release/norn".to_string()),
        ledger,
        suite_filter,
    })
}

/// Resolve `raw` (a bare name or a path) to a stable absolute path.
///
/// Every case sets `current_dir` to the fixture vault before spawning, so a
/// *relative* program path resolved lazily at spawn time would break after
/// the first case (or resolve against the wrong directory entirely).
/// Resolving to an absolute path once, up front, sidesteps that regardless
/// of platform relative-path/chdir ordering.
fn resolve_binary(raw: &str) -> Result<PathBuf, String> {
    if raw.contains('/') || raw.contains(std::path::MAIN_SEPARATOR) {
        return fs::canonicalize(raw).map_err(|e| format!("{raw}: {e}"));
    }
    let path_var = std::env::var_os("PATH").ok_or("PATH is not set")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(raw);
        if candidate.is_file() {
            return fs::canonicalize(&candidate)
                .map_err(|e| format!("{}: {e}", candidate.display()));
        }
    }
    Err(format!("`{raw}` not found on PATH"))
}

/// Walk up from `start` to the ancestor directory whose `Cargo.toml`
/// declares `[workspace]` — the default `--ledger` path is
/// `<that dir>/docs/parity-ledger.toml`.
fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        let candidate = dir.join("Cargo.toml");
        if candidate.is_file() {
            if let Ok(text) = fs::read_to_string(&candidate) {
                if text.contains("[workspace]") {
                    return Some(dir);
                }
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn default_ledger_path() -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cannot read cwd: {e}"))?;
    find_workspace_root(&cwd)
        .map(|root| root.join("docs/parity-ledger.toml"))
        .ok_or_else(|| {
            format!(
                "could not find a workspace root (an ancestor of {} with a Cargo.toml declaring [workspace]) to resolve the default --ledger path",
                cwd.display()
            )
        })
}

fn run_consistency(args: &Args) -> ExitCode {
    let oracle_path = match resolve_binary(&args.oracle) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("norn-parity: oracle binary: {e}");
            return ExitCode::from(2);
        }
    };
    match consistency::run(&oracle_path) {
        Ok(findings) if findings.is_empty() => {
            println!("consistency: 0 disagreements");
            ExitCode::SUCCESS
        }
        Ok(findings) => {
            for f in &findings {
                println!(
                    "disagreement [{}] fixture={}: {}",
                    f.check, f.fixture, f.message
                );
            }
            eprintln!(
                "norn-parity: {} oracle self-consistency disagreement(s) — candidate divergence-ledger entries (ADR 0018)",
                findings.len()
            );
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("norn-parity: {e}");
            ExitCode::from(2)
        }
    }
}

fn run_comparison(args: &Args, mode: Mode) -> ExitCode {
    let oracle_path = match resolve_binary(&args.oracle) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("norn-parity: oracle binary: {e}");
            return ExitCode::from(2);
        }
    };
    let rewrite_path = match resolve_binary(&args.rewrite) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("norn-parity: rewrite binary: {e}");
            return ExitCode::from(2);
        }
    };
    let ledger_path = match &args.ledger {
        Some(p) => p.clone(),
        None => match default_ledger_path() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("norn-parity: {e}");
                return ExitCode::from(2);
            }
        },
    };

    let config = RunConfig {
        mode,
        oracle: &oracle_path,
        rewrite: &rewrite_path,
        ledger_path: &ledger_path,
        suite_filter: &args.suite_filter,
    };

    match run::run(&config) {
        Ok(rep) => {
            print!("{}", report::render(&rep, mode));
            ExitCode::from(rep.exit_code())
        }
        Err(e) => {
            eprintln!("norn-parity: {e}");
            ExitCode::from(2)
        }
    }
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse_args(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("norn-parity: {e}\n\n{}", usage());
            return ExitCode::from(2);
        }
    };

    match args.mode {
        ModeFlag::Consistency => run_consistency(&args),
        ModeFlag::SelfCheck => run_comparison(&args, Mode::SelfCheck),
        ModeFlag::All => run_comparison(&args, Mode::All),
        ModeFlag::Gated => run_comparison(&args, Mode::Gated),
    }
}
