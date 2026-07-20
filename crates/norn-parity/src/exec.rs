//! Running one binary with one case's argv/stdin/cwd and capturing its
//! stdout/stderr/exit code.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
    /// The child did not exit within the bound passed to
    /// [`run_argv_bounded`] — killed and reaped (never left running or a
    /// zombie) rather than hanging the runner. Only that function can
    /// produce this; plain [`run_argv`]/[`run_case`] wait unboundedly, as
    /// every non-MCP case's process naturally exits after doing its work.
    Timeout { binary: String, timeout: Duration },
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
            ExecError::Timeout { binary, timeout } => {
                write!(
                    f,
                    "{binary} did not exit within {timeout:?} — killed and reaped"
                )
            }
        }
    }
}

impl std::error::Error for ExecError {}

/// A spawned child plus its (optional) stdin-writer thread — the setup
/// [`run_argv`] and [`run_argv_bounded`] share; only how they WAIT for the
/// child differs (unbounded `wait_with_output` vs. a polled, killable
/// deadline), so that is the one thing left to each caller.
struct Spawned {
    child: std::process::Child,
    stdin_writer: Option<std::thread::JoinHandle<std::io::Result<()>>>,
}

/// Spawn `binary` with `argv`/cwd = `vault` (stdin/stdout/stderr all piped),
/// and — if `stdin` is `Some` — start writing it on a dedicated thread
/// rather than inline before the caller drains output. An inline
/// `write_all` deadlocks once the payload plus the child's own output
/// exceed the OS pipe buffers (~64KB): the child blocks writing stdout
/// while we block writing stdin, and neither side drains the other.
fn spawn_with_stdin(
    binary: &Path,
    argv: &[&str],
    stdin: Option<&str>,
    vault: &Path,
    binary_label: &str,
) -> Result<Spawned, ExecError> {
    let mut child = Command::new(binary)
        .args(argv)
        .current_dir(vault)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| ExecError::Spawn {
            binary: binary_label.to_string(),
            source,
        })?;

    let stdin_writer = if let Some(stdin_text) = stdin {
        // `.expect` on the piped handle is safe: we just requested it above.
        let mut child_stdin = child.stdin.take().expect("stdin was piped");
        let bytes = stdin_text.as_bytes().to_vec();
        Some(std::thread::spawn(move || {
            // Drop (at end of closure) closes the pipe so the child sees EOF.
            child_stdin.write_all(&bytes)
        }))
    } else {
        drop(child.stdin.take());
        None
    };

    Ok(Spawned {
        child,
        stdin_writer,
    })
}

/// Run `binary` with `case`'s argv, cwd = `vault` — no `-C` flag, so the
/// identical argv drives both the oracle and the rewrite binary and
/// normalization never has to strip the vault path out of argv itself.
///
/// An MCP case (`case.stdin.is_some()`) is never driven through here: its
/// frames need `crate::mcp::run_case`'s bounded driving (a stub or a real
/// bug could otherwise hang the runner reading stdin that never arrives) and
/// its frame-by-frame JSON comparison, not this raw byte comparison — see
/// `crate::run::run_suites`, which branches before reaching this function.
pub fn run_case(binary: &Path, case: &Case, vault: &Path) -> Result<RawOutput, ExecError> {
    debug_assert!(
        case.stdin.is_none(),
        "an MCP case (stdin: Some) must be driven by crate::mcp::run_case, not exec::run_case"
    );
    run_argv(binary, case.argv, None, vault)
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
    let Spawned {
        child,
        stdin_writer,
    } = spawn_with_stdin(binary, argv, stdin, vault, &binary_label)?;

    // `wait_with_output` drains stdout/stderr concurrently with the stdin
    // writer thread (spawned above) — waiting AND draining together is
    // exactly what avoids the deadlock `spawn_with_stdin`'s doc describes.
    let output = child.wait_with_output().map_err(|source| ExecError::Wait {
        binary: binary_label.clone(),
        source,
    })?;

    if let Some(handle) = stdin_writer {
        handle
            .join()
            .expect("stdin writer thread panicked")
            .map_err(|source| ExecError::Stdin {
                binary: binary_label,
                source,
            })?;
    }

    Ok(RawOutput {
        stdout: output.stdout,
        stderr: output.stderr,
        exit_code: output.status.code(),
    })
}

/// Like [`run_argv`], but bounds the child's wall-clock lifetime: if it has
/// not exited by `timeout`, it is killed and reaped (never left running or a
/// zombie) and this returns `Err(ExecError::Timeout{..})` instead of hanging
/// the runner. MCP frame driving (`crate::mcp`) is the one case shape that
/// needs this — a stub or a real bug can block forever reading stdin that
/// never arrives, or writing responses nobody reads — where every other
/// case's process naturally exits once it has done its work, so plain
/// `run_argv`'s unbounded `wait_with_output` is fine there.
///
/// Cannot reuse `wait_with_output` (which blocks until exit, precisely what
/// a timeout must not do): stdout/stderr are drained on their own reader
/// threads instead, mirroring the stdin-writer-thread deadlock-avoidance
/// reasoning in [`spawn_with_stdin`] — the main thread only ever polls
/// `try_wait`, never blocks on the child.
pub fn run_argv_bounded(
    binary: &Path,
    argv: &[&str],
    stdin: Option<&str>,
    vault: &Path,
    timeout: Duration,
) -> Result<RawOutput, ExecError> {
    const POLL_INTERVAL: Duration = Duration::from_millis(20);

    let binary_label = binary.display().to_string();
    let Spawned {
        mut child,
        stdin_writer,
    } = spawn_with_stdin(binary, argv, stdin, vault, &binary_label)?;

    let mut child_stdout = child.stdout.take().expect("stdout was piped");
    let stdout_reader = std::thread::spawn(move || -> Vec<u8> {
        let mut buf = Vec::new();
        let _ = child_stdout.read_to_end(&mut buf);
        buf
    });
    let mut child_stderr = child.stderr.take().expect("stderr was piped");
    let stderr_reader = std::thread::spawn(move || -> Vec<u8> {
        let mut buf = Vec::new();
        let _ = child_stderr.read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait().map_err(|source| ExecError::Wait {
            binary: binary_label.clone(),
            source,
        })? {
            Some(status) => break Some(status),
            None => {
                if Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        }
    };

    let status = match status {
        Some(status) => status,
        None => {
            // Deadline hit before the child exited: kill and reap so it
            // never becomes a zombie. Killing OUR OWN CHILD closes its end
            // of every pipe, which unblocks a stdin writer mid-`write_all`
            // (EPIPE) and lets the reader threads observe EOF — so on THIS
            // (timeout) path, every thread below is guaranteed to finish,
            // never hang this join. (That guarantee is specific to this
            // path: it holds because we hold the exact pid we spawned and
            // kill it directly. It is NOT a general guarantee for the
            // normal-exit path below — see the comment there.)
            let _ = child.kill();
            let _ = child.wait();
            if let Some(handle) = stdin_writer {
                let _ = handle.join();
            }
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(ExecError::Timeout {
                binary: binary_label,
                timeout,
            });
        }
    };

    if let Some(handle) = stdin_writer {
        handle
            .join()
            .expect("stdin writer thread panicked")
            .map_err(|source| ExecError::Stdin {
                binary: binary_label.clone(),
                source,
            })?;
    }
    // The child itself has already exited (`status` above), so its own copy
    // of each pipe's write end is closed — but that alone only guarantees
    // these joins finish if NO OTHER process holds a duplicate of that fd.
    // A grandchild the child spawned and left running, inheriting the pipe,
    // could still keep a reader blocked here indefinitely; unreachable for
    // the real `norn mcp` (a single process, no children) and for every
    // stub this crate's tests use (each closes/replaces its own image
    // rather than forking a lingering descendant — see `tests/mcp.rs`'s
    // `exec sleep` stub for the general shape of that hazard elsewhere).
    let stdout = stdout_reader.join().expect("stdout reader thread panicked");
    let stderr = stderr_reader.join().expect("stderr reader thread panicked");

    Ok(RawOutput {
        stdout,
        stderr,
        exit_code: status.code(),
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
