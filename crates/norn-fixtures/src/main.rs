#![forbid(unsafe_code)]
//! Thin CLI wrapper over `norn_fixtures::generate`. Hand-parsed argv — no
//! `clap` — per the crate's zero-runtime-dependency constraint.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use norn_fixtures::Profile;

const SENTINEL: &str = ".norn-fixture-vault";

fn usage() -> String {
    format!(
        "usage: norn-fixtures <out-dir> --profile <name> [--seed N]\n\navailable profiles: {}",
        Profile::names().join(", ")
    )
}

struct Args {
    out_dir: PathBuf,
    profile: String,
    seed: u64,
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut out_dir: Option<PathBuf> = None;
    let mut profile: Option<String> = None;
    let mut seed: u64 = 0;

    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--profile" => {
                i += 1;
                let v = argv.get(i).ok_or("--profile requires a value")?;
                profile = Some(v.clone());
            }
            "--seed" => {
                i += 1;
                let v = argv.get(i).ok_or("--seed requires a value")?;
                seed = v
                    .parse::<u64>()
                    .map_err(|_| format!("--seed value is not a valid u64: {v}"))?;
            }
            other if out_dir.is_none() => {
                out_dir = Some(PathBuf::from(other));
            }
            other => {
                return Err(format!("unexpected argument: {other}"));
            }
        }
        i += 1;
    }

    let out_dir = out_dir.ok_or("missing required <out-dir> argument")?;
    let profile = profile.ok_or("missing required --profile <name>")?;
    Ok(Args {
        out_dir,
        profile,
        seed,
    })
}

/// Mimir's sentinel-guard pattern: absent/empty out-dir generates fresh;
/// present with the sentinel is safe to clear and regenerate; present
/// without it is refused outright (no `--force` escape hatch).
fn prepare_target(out_dir: &Path) -> Result<(), String> {
    let meta = match fs::metadata(out_dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("cannot stat {}: {e}", out_dir.display())),
    };
    if !meta.is_dir() {
        return Err(format!(
            "{} exists and is not a directory",
            out_dir.display()
        ));
    }
    let mut entries =
        fs::read_dir(out_dir).map_err(|e| format!("cannot read {}: {e}", out_dir.display()))?;
    if entries.next().is_none() {
        return Ok(());
    }
    if !out_dir.join(SENTINEL).exists() {
        return Err(format!(
            "{} exists, is non-empty, and has no {SENTINEL} sentinel — refusing to touch it",
            out_dir.display()
        ));
    }
    // Sentinel present: safe to clear and regenerate.
    for entry in
        fs::read_dir(out_dir).map_err(|e| format!("cannot read {}: {e}", out_dir.display()))?
    {
        let entry =
            entry.map_err(|e| format!("cannot read entry in {}: {e}", out_dir.display()))?;
        let path = entry.path();
        let result = if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        result.map_err(|e| format!("cannot remove {}: {e}", path.display()))?;
    }
    Ok(())
}

fn run() -> Result<String, (String, u8)> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = parse_args(&argv).map_err(|e| (format!("{e}\n\n{}", usage()), 2))?;

    let profile = Profile::by_name(&args.profile).ok_or_else(|| {
        (
            format!("unknown profile '{}'\n\n{}", args.profile, usage()),
            2,
        )
    })?;

    prepare_target(&args.out_dir).map_err(|e| (e, 1))?;

    let summary = norn_fixtures::generate(&profile, args.seed, &args.out_dir)
        .map_err(|e| (format!("generation failed: {e}"), 1))?;

    Ok(format!(
        "generated {} vault (seed {}): {} docs, {} files -> {}",
        profile.name,
        args.seed,
        summary.docs,
        summary.files,
        args.out_dir.display()
    ))
}

fn main() -> ExitCode {
    match run() {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err((message, code)) => {
            eprintln!("norn-fixtures: {message}");
            ExitCode::from(code)
        }
    }
}
