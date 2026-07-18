//! Running one binary with one case's argv/stdin/cwd and capturing its
//! stdout/stderr/exit code.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::cases::Case;

/// A captured process outcome, pre-normalization.
pub struct RawOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// `None` when the process was terminated by a signal rather than
    /// exiting — treated as a runner error by the caller, never a verdict.
    pub exit_code: Option<i32>,
}

#[derive(Debug)]
pub enum ExecError {
    Spawn {
        binary: String,
        source: std::io::Error,
    },
    Stdin {
        binary: String,
        source: std::io::Error,
    },
    Wait {
        binary: String,
        source: std::io::Error,
    },
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::Spawn { binary, source } => {
                write!(f, "failed to spawn {binary}: {source}")
            }
            ExecError::Stdin { binary, source } => {
                write!(f, "failed to write stdin to {binary}: {source}")
            }
            ExecError::Wait { binary, source } => {
                write!(f, "failed to wait on {binary}: {source}")
            }
        }
    }
}

impl std::error::Error for ExecError {}

/// Run `binary` with `case`'s argv/stdin, cwd = `vault` — no `-C` flag, so
/// the identical argv drives both the oracle and the rewrite binary and
/// normalization never has to strip the vault path out of argv itself.
pub fn run_case(binary: &Path, case: &Case, vault: &Path) -> Result<RawOutput, ExecError> {
    run_argv(binary, case.argv, case.stdin, vault)
}

/// Lower-level than [`run_case`]: run arbitrary `argv`/`stdin` against
/// `binary` with cwd = `vault`. Used directly by the oracle
/// self-consistency checks (`crate::consistency`), which cross-check
/// commands that are not declared as parity [`Case`]s.
pub fn run_argv(
    binary: &Path,
    argv: &[&str],
    stdin: Option<&str>,
    vault: &Path,
) -> Result<RawOutput, ExecError> {
    let binary_label = binary.display().to_string();
    let mut child = Command::new(binary)
        .args(argv)
        .current_dir(vault)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| ExecError::Spawn {
            binary: binary_label.clone(),
            source,
        })?;

    if let Some(stdin_text) = stdin {
        // `.expect` on the piped handle is safe: we just requested it above.
        let mut child_stdin = child.stdin.take().expect("stdin was piped");
        child_stdin
            .write_all(stdin_text.as_bytes())
            .map_err(|source| ExecError::Stdin {
                binary: binary_label.clone(),
                source,
            })?;
        // Drop closes the pipe so the child sees EOF.
    } else {
        drop(child.stdin.take());
    }

    let output = child.wait_with_output().map_err(|source| ExecError::Wait {
        binary: binary_label,
        source,
    })?;

    Ok(RawOutput {
        stdout: output.stdout,
        stderr: output.stderr,
        exit_code: output.status.code(),
    })
}

/// Probe `binary --version`, tolerating any exit code — callers decide how
/// strict to be (the oracle's version must succeed and match the ledger's
/// pinned version; the phase-0 rewrite skeleton's `--version` exits 2 with
/// a notice, and only its existence is required).
pub fn probe_version(binary: &Path) -> Result<RawOutput, ExecError> {
    let binary_label = binary.display().to_string();
    let output = Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map_err(|source| ExecError::Spawn {
            binary: binary_label,
            source,
        })?;
    Ok(RawOutput {
        stdout: output.stdout,
        stderr: output.stderr,
        exit_code: output.status.code(),
    })
}
