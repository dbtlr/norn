//! Per-shell completion install support.
//!
//! `vault completions install [shell]` wires shell completions into the
//! user's shell config in a single command, idempotently. The dispatch
//! lives here; each shell's specifics live in a sibling module.

use std::fs;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};

use crate::cli::{CompletionsInstallArgs, SupportedShell};

mod bash;
mod elvish;
mod fish;
mod nushell;
mod powershell;
mod zsh;

/// Marker comment prefix written into rc files to enable idempotent re-install.
pub(crate) const MARKER_PREFIX: &str = "# >>> vault completions";
pub(crate) const MARKER_SUFFIX: &str = "# <<< vault completions <<<";

/// Outcome of an install run, for reporting and tests.
#[derive(Debug)]
pub enum InstallOutcome {
    /// Wrote the marker block (and possibly other files) anew.
    Wrote {
        primary_target: Utf8PathBuf,
        secondary_target: Option<Utf8PathBuf>,
        backup: Option<Utf8PathBuf>,
        line_range: Option<(usize, usize)>,
    },
    /// Marker block was already present; nothing changed.
    AlreadyInstalled {
        primary_target: Utf8PathBuf,
        line: usize,
    },
    /// `--force` replaced an existing marker block.
    Replaced {
        primary_target: Utf8PathBuf,
        secondary_target: Option<Utf8PathBuf>,
        backup: Option<Utf8PathBuf>,
        line_range: Option<(usize, usize)>,
    },
    /// `--print` produced preview output without writing.
    Previewed {
        primary_target: Utf8PathBuf,
        secondary_target: Option<Utf8PathBuf>,
    },
}

/// Per-shell install behavior.
pub(crate) trait Installer {
    /// Human-readable shell name for output.
    fn shell_name(&self) -> &'static str;

    /// Compute the target file(s) for this shell, resolving env vars and
    /// platform defaults.
    fn target_paths(&self) -> Result<TargetPaths>;

    /// Produce the bytes to write into the primary target. For most shells
    /// this is a marker block containing an eval/source line. For fish it
    /// is the full completion script. For nushell it is the marker block
    /// containing the source line that references the secondary target.
    fn primary_content(&self, today: &str) -> Result<String>;

    /// Produce the bytes to write into the secondary target, if any.
    /// Only nushell uses this (the script file).
    fn secondary_content(&self) -> Result<Option<String>>;

    /// Whether this shell uses a marker block in its primary target.
    /// Fish writes a full script and skips marker detection; the others
    /// append a marker block.
    fn uses_marker_block(&self) -> bool {
        true
    }
}

/// Resolved target paths for an installer.
#[derive(Debug, Clone)]
pub(crate) struct TargetPaths {
    pub primary: Utf8PathBuf,
    pub secondary: Option<Utf8PathBuf>,
}

/// Entry point invoked from completions::run_install.
pub fn run(args: CompletionsInstallArgs) -> Result<InstallOutcome> {
    let shell = resolve_shell(args.shell)?;
    let installer: Box<dyn Installer> = match shell {
        SupportedShell::Bash => Box::new(bash::BashInstaller),
        SupportedShell::Zsh => Box::new(zsh::ZshInstaller),
        SupportedShell::Fish => Box::new(fish::FishInstaller),
        SupportedShell::Powershell => Box::new(powershell::PowershellInstaller),
        SupportedShell::Elvish => Box::new(elvish::ElvishInstaller),
        SupportedShell::Nushell => Box::new(nushell::NushellInstaller),
    };

    let targets = installer.target_paths()?;
    let today = today_ymd();
    let primary = installer.primary_content(&today)?;
    let secondary = installer.secondary_content()?;

    if args.print {
        print_preview(
            installer.shell_name(),
            &targets,
            &primary,
            secondary.as_deref(),
        )?;
        return Ok(InstallOutcome::Previewed {
            primary_target: targets.primary.clone(),
            secondary_target: targets.secondary.clone(),
        });
    }

    execute(
        installer.as_ref(),
        &targets,
        &primary,
        secondary.as_deref(),
        args.force,
    )
}

fn resolve_shell(explicit: Option<SupportedShell>) -> Result<SupportedShell> {
    if let Some(shell) = explicit {
        return Ok(shell);
    }
    let env_shell = std::env::var("SHELL").context(
        "could not auto-detect shell: $SHELL is not set. Pass a shell explicitly, e.g. `vault completions install zsh`."
    )?;
    let name = Utf8Path::new(env_shell.as_str()).file_name().unwrap_or("");
    match name {
        "bash" => Ok(SupportedShell::Bash),
        "zsh" => Ok(SupportedShell::Zsh),
        "fish" => Ok(SupportedShell::Fish),
        "pwsh" | "powershell" => Ok(SupportedShell::Powershell),
        "elvish" => Ok(SupportedShell::Elvish),
        "nu" | "nushell" => Ok(SupportedShell::Nushell),
        other => bail!(
            "could not auto-detect shell from $SHELL={env_shell}: '{other}' is not in the supported set (bash, zsh, fish, powershell, elvish, nushell). Pass a shell explicitly."
        ),
    }
}

fn today_ymd() -> String {
    // Best-effort YYYY-MM-DD; uses std::time::SystemTime so we don't take a
    // chrono dependency just for the marker comment date.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Days since epoch
    let days = (secs / 86_400) as i64;
    // Convert days since 1970-01-01 to (Y, M, D) using a small algorithm.
    let (y, m, d) = epoch_days_to_ymd(days);
    format!("{y:04}-{m:02}-{d:02}")
}

fn epoch_days_to_ymd(days_since_epoch: i64) -> (i32, u32, u32) {
    // Days since 1970-01-01.
    // Algorithm from Howard Hinnant's date library, public domain.
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = (if m <= 2 { y + 1 } else { y }) as i32;
    (y, m, d)
}

fn print_preview(
    shell_name: &str,
    targets: &TargetPaths,
    primary: &str,
    secondary: Option<&str>,
) -> Result<()> {
    println!("Shell: {shell_name}");
    println!("Would write to: {}", targets.primary);
    if let Some(sec) = &targets.secondary {
        println!("Would write secondary to: {sec}");
    }
    println!();
    if let Some(sec) = secondary {
        println!(
            "--- {} ---",
            targets
                .secondary
                .as_ref()
                .expect("secondary content implies secondary target")
        );
        println!("{sec}");
        println!();
    }
    println!("--- {} ---", targets.primary);
    println!("{primary}");
    Ok(())
}

fn execute(
    installer: &dyn Installer,
    targets: &TargetPaths,
    primary: &str,
    secondary: Option<&str>,
    force: bool,
) -> Result<InstallOutcome> {
    // Write secondary first (script file for nushell, no-op for others).
    if let (Some(sec_path), Some(sec_content)) = (&targets.secondary, secondary) {
        ensure_parent_dir(sec_path)?;
        fs::write(sec_path.as_std_path(), sec_content)
            .with_context(|| format!("writing secondary target {sec_path}"))?;
    }

    if !installer.uses_marker_block() {
        // Fish path: always overwrite primary file with the script.
        ensure_parent_dir(&targets.primary)?;
        fs::write(targets.primary.as_std_path(), primary)
            .with_context(|| format!("writing {}", targets.primary))?;
        return Ok(InstallOutcome::Wrote {
            primary_target: targets.primary.clone(),
            secondary_target: targets.secondary.clone(),
            backup: None,
            line_range: None,
        });
    }

    // Marker-block path: read existing file, detect marker, decide action.
    ensure_parent_dir(&targets.primary)?;
    let original = read_file_or_empty(&targets.primary)?;
    let marker_range = find_marker_block(&original);

    match (marker_range, force) {
        (Some((start_line, _end_line)), false) => Ok(InstallOutcome::AlreadyInstalled {
            primary_target: targets.primary.clone(),
            line: start_line + 1, // 1-indexed for human output
        }),
        (Some(range), true) => {
            let backup = write_backup(&targets.primary, &original)?;
            let updated = replace_marker_block(&original, range, primary);
            fs::write(targets.primary.as_std_path(), &updated)
                .with_context(|| format!("writing {}", targets.primary))?;
            let new_range = find_marker_block(&updated).map(|(s, e)| (s + 1, e + 1));
            Ok(InstallOutcome::Replaced {
                primary_target: targets.primary.clone(),
                secondary_target: targets.secondary.clone(),
                backup: Some(backup),
                line_range: new_range,
            })
        }
        (None, _) => {
            // Append marker block.
            let backup = if !original.is_empty() {
                Some(write_backup(&targets.primary, &original)?)
            } else {
                None
            };
            let updated = append_marker_block(&original, primary);
            fs::write(targets.primary.as_std_path(), &updated)
                .with_context(|| format!("writing {}", targets.primary))?;
            let new_range = find_marker_block(&updated).map(|(s, e)| (s + 1, e + 1));
            Ok(InstallOutcome::Wrote {
                primary_target: targets.primary.clone(),
                secondary_target: targets.secondary.clone(),
                backup,
                line_range: new_range,
            })
        }
    }
}

pub(crate) fn ensure_parent_dir(path: &Utf8Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent.as_std_path())
            .with_context(|| format!("creating parent dir {parent}"))?;
    }
    Ok(())
}

pub(crate) fn read_file_or_empty(path: &Utf8Path) -> Result<String> {
    match fs::read_to_string(path.as_std_path()) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(anyhow!("reading {path}: {e}")),
    }
}

fn write_backup(path: &Utf8Path, content: &str) -> Result<Utf8PathBuf> {
    let backup_path = Utf8PathBuf::from(format!("{path}.bak"));
    fs::write(backup_path.as_std_path(), content)
        .with_context(|| format!("writing backup {backup_path}"))?;
    Ok(backup_path)
}

/// Returns the (start_line, end_line) range of the marker block in `content`,
/// 0-indexed, inclusive of both markers. None if no marker found.
pub(crate) fn find_marker_block(content: &str) -> Option<(usize, usize)> {
    let lines: Vec<&str> = content.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.trim_start().starts_with(MARKER_PREFIX))?;
    let end = lines
        .iter()
        .enumerate()
        .skip(start)
        .find(|(_, l)| l.trim_start().starts_with(MARKER_SUFFIX))
        .map(|(i, _)| i)?;
    Some((start, end))
}

fn append_marker_block(original: &str, block: &str) -> String {
    let mut result = String::with_capacity(original.len() + block.len() + 2);
    result.push_str(original);
    if !result.is_empty() && !result.ends_with('\n') {
        result.push('\n');
    }
    if !result.is_empty() {
        result.push('\n');
    }
    result.push_str(block);
    if !result.ends_with('\n') {
        result.push('\n');
    }
    result
}

fn replace_marker_block(original: &str, range: (usize, usize), block: &str) -> String {
    let lines: Vec<&str> = original.lines().collect();
    let (start, end) = range;
    let mut result = String::with_capacity(original.len());
    for (i, line) in lines.iter().enumerate() {
        if i == start {
            result.push_str(block);
            if !result.ends_with('\n') {
                result.push('\n');
            }
        }
        if !(start..=end).contains(&i) {
            result.push_str(line);
            result.push('\n');
        }
    }
    // Preserve trailing newline of the original.
    if !original.ends_with('\n') {
        result.pop();
    }
    result
}

pub(crate) fn home_dir() -> Result<Utf8PathBuf> {
    let home = std::env::var("HOME")
        .context("$HOME is not set; required to resolve shell config paths")?;
    Utf8PathBuf::try_from(PathBuf::from(home)).context("$HOME is not valid UTF-8")
}

pub(crate) fn xdg_config_home() -> Result<Utf8PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return Utf8PathBuf::try_from(PathBuf::from(xdg))
            .context("$XDG_CONFIG_HOME is not valid UTF-8");
    }
    Ok(home_dir()?.join(".config"))
}

pub fn render_outcome(outcome: &InstallOutcome) -> String {
    match outcome {
        InstallOutcome::Wrote {
            primary_target,
            secondary_target,
            backup,
            line_range,
        } => {
            let mut s = format!("Wrote completion block to {primary_target}.\n");
            if let Some(sec) = secondary_target {
                s.push_str(&format!("Wrote completion script to {sec}.\n"));
            }
            if let Some((start, end)) = line_range {
                s.push_str(&format!("Block at lines {start}-{end}.\n"));
            }
            if let Some(b) = backup {
                s.push_str(&format!("Backup saved to {b}.\n"));
            }
            s
        }
        InstallOutcome::AlreadyInstalled {
            primary_target,
            line,
        } => {
            format!(
                "Already installed at line {line} of {primary_target}. Use --force to overwrite.\n"
            )
        }
        InstallOutcome::Replaced {
            primary_target,
            secondary_target,
            backup,
            line_range,
        } => {
            let mut s = format!("Replaced completion block in {primary_target}.\n");
            if let Some(sec) = secondary_target {
                s.push_str(&format!("Wrote completion script to {sec}.\n"));
            }
            if let Some((start, end)) = line_range {
                s.push_str(&format!("New block at lines {start}-{end}.\n"));
            }
            if let Some(b) = backup {
                s.push_str(&format!("Backup saved to {b}.\n"));
            }
            s
        }
        InstallOutcome::Previewed {
            primary_target,
            secondary_target,
        } => {
            let mut s = format!("Previewed (no writes). Primary target: {primary_target}.\n");
            if let Some(sec) = secondary_target {
                s.push_str(&format!("Secondary target: {sec}.\n"));
            }
            s
        }
    }
}
