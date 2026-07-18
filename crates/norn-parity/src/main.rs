#![forbid(unsafe_code)]
//! `norn-parity [--self-check | --all | --consistency] --oracle <path>
//! [--rewrite <path>] [--ledger <path>] [--suite <name>]...`
//!
//! Hand-parsed argv (std::env::args) — no `clap`, matching `norn-fixtures`'
//! bin (see `crates/norn-fixtures/src/main.rs`).

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use norn_parity::ledger::Ledger;
use norn_parity::run::{self, Mode, RunConfig};
use norn_parity::{cases, consistency, exec, paths, report};

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

fn default_ledger_path() -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cannot read cwd: {e}"))?;
    paths::find_workspace_root(&cwd)
        .map(|root| root.join("docs/parity-ledger.toml"))
        .ok_or_else(|| {
            format!(
                "could not find a workspace root (an ancestor of {} with a Cargo.toml declaring [workspace]) to resolve the default --ledger path",
                cwd.display()
            )
        })
}

/// Resolve a binary path or map the failure to an exit-2 code, emitting the
/// standard `norn-parity: {label} binary: {e}` diagnostic — the one home for
/// the three verbatim resolve-or-exit blocks the modes shared.
fn resolve_or_exit(raw: &str, label: &str) -> Result<PathBuf, ExitCode> {
    resolve_binary(raw).map_err(|e| {
        eprintln!("norn-parity: {label} binary: {e}");
        ExitCode::from(2)
    })
}

fn resolve_ledger_or_exit(args: &Args) -> Result<PathBuf, ExitCode> {
    match &args.ledger {
        Some(p) => Ok(p.clone()),
        None => default_ledger_path().map_err(|e| {
            eprintln!("norn-parity: {e}");
            ExitCode::from(2)
        }),
    }
}

/// Probe the oracle's `--version`, require success, and return its
/// semver-shaped token — the pin the ledger's `meta.oracle_version` must
/// match.
fn oracle_version_or_exit(oracle: &Path) -> Result<String, ExitCode> {
    let raw = exec::probe_version(oracle).map_err(|e| {
        eprintln!("norn-parity: oracle --version failed: {e}");
        ExitCode::from(2)
    })?;
    if raw.exit_code != Some(0) {
        eprintln!(
            "norn-parity: oracle --version did not succeed (exit {:?})",
            raw.exit_code
        );
        return Err(ExitCode::from(2));
    }
    let stdout = String::from_utf8_lossy(&raw.stdout);
    run::parse_version_token(&stdout).ok_or_else(|| {
        eprintln!("norn-parity: oracle --version produced no semver-shaped token: {stdout:?}");
        ExitCode::from(2)
    })
}

fn run_consistency(args: &Args) -> ExitCode {
    let oracle_path = match resolve_or_exit(&args.oracle, "oracle") {
        Ok(p) => p,
        Err(code) => return code,
    };

    // Consistency is a pin-scoped trust claim (its invariants describe the
    // pinned oracle) — so it loads the ledger meta and enforces the pin, like
    // the comparison modes, even though it consults no entries and runs no
    // cases (ADR 0018 mode/ledger/pin matrix).
    let oracle_version = match oracle_version_or_exit(&oracle_path) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let ledger_path = match resolve_ledger_or_exit(args) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let known: BTreeSet<&str> = cases::all_case_ids().into_iter().collect();
    let ported: BTreeSet<&str> = cases::ported_case_ids().into_iter().collect();
    let ledger = match Ledger::load(&ledger_path, &known, &ported) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("norn-parity: {e}");
            return ExitCode::from(2);
        }
    };
    if let Err(e) = ledger.check_oracle_version(&oracle_version) {
        eprintln!("norn-parity: {e}");
        return ExitCode::from(2);
    }

    match consistency::run(&oracle_path) {
        Ok(findings) => {
            let (stdout, exit) = report::render_consistency(&findings);
            print!("{stdout}");
            if exit != 0 {
                eprintln!(
                    "norn-parity: {} oracle self-consistency disagreement(s) — candidate divergence-ledger entries (ADR 0018)",
                    findings.len()
                );
            }
            ExitCode::from(exit)
        }
        Err(e) => {
            eprintln!("norn-parity: {e}");
            ExitCode::from(2)
        }
    }
}

fn run_comparison(args: &Args, mode: Mode) -> ExitCode {
    let oracle_path = match resolve_or_exit(&args.oracle, "oracle") {
        Ok(p) => p,
        Err(code) => return code,
    };
    // Self-check runs oracle-vs-oracle; the rewrite binary is unused and its
    // absence must not block the mode's whole purpose (vetting a case set
    // before any rewrite artifact exists).
    let rewrite_path = if matches!(mode, Mode::SelfCheck) {
        oracle_path.clone()
    } else {
        match resolve_or_exit(&args.rewrite, "rewrite") {
            Ok(p) => p,
            Err(code) => return code,
        }
    };
    // Self-check is ledger-blind: it must run without any ledger (so a case
    // set can be vetted against a NEW oracle before the ledger is updated),
    // so its default-path resolution is skipped entirely. Gated/all resolve
    // and load the ledger.
    let ledger_path = if matches!(mode, Mode::SelfCheck) {
        PathBuf::new()
    } else {
        match resolve_ledger_or_exit(args) {
            Ok(p) => p,
            Err(code) => return code,
        }
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
