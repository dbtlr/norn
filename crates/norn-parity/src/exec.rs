//! Running one binary with one case's argv/stdin/cwd and capturing its
//! stdout/stderr/exit code.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::cases::Case;

/// The environment every binary this module spawns runs under.
///
/// The caller's environment is CLEARED and rebuilt from a fixed allowlist.
/// Anything a host carries that reaches a case is measured as a difference:
/// a `norn serve` daemon the client finds through `$HOME` adds a version-skew
/// line to one side's stderr, and a leaked `NORN_ROOT` points a case at a
/// vault that is not its fixture. Neither is a property of the two binaries,
/// which is the only thing a parity run is entitled to measure.
///
/// "Both sides get the same value" is NOT enough to forward a variable. The
/// two binaries do not read the environment the same way, so a variable one
/// side branches on and the other ignores turns the host into an input:
///
/// - the LOCALE is pinned, not forwarded. The rewrite selects glyphs from
///   `LC_ALL` -> `LC_CTYPE` -> `LANG` and falls back to ASCII off a
///   non-UTF-8 locale (`norn-cli`'s `output::glyphs`); the pinned oracle
///   emits unicode unconditionally. Forwarding the host's locale therefore
///   makes several cases differ on a developer's machine and match on a CI
///   image, and silently bakes whatever locale the tables were recorded
///   under into the ledger. `LC_ALL=C.UTF-8` is set and `LANG`/`LC_CTYPE`
///   are removed, so both sides always render the same glyph set. The
///   rewrite's adaptive fallback is deliberately not exercised by parity —
///   see the locale ruling in `docs/parity-ledger.toml`'s header;
/// - `HOME` and the XDG bases point into a scratch tree the run owns, so a
///   registry, cache, config, socket or log a binary creates for itself is
///   created fresh and thrown away with the run. `XDG_RUNTIME_DIR` gets its
///   own SHORT temp dir rather than a path under the fixture cache: it holds
///   the `AF_UNIX` socket a summoned vault owner binds, and a fixture-cache
///   path plus that socket's name overruns `sun_path` — see
///   `norn_fixtures::testing::short_runtime_dir`, which picks the base and
///   refuses a path that could not work;
/// - `NORN_ROOT` and `NORN_CONFIG_DIR` are removed explicitly after the
///   allowlist is applied. `env_clear` already drops them; the explicit
///   removal keeps them dropped if the allowlist ever widens.
/// - `NORN_EPHEMERAL_TTL_SECS` is forced to [`OWNER_IDLE_TTL_SECS`]. There is
///   no direct/no-daemon path (ADR 0017 reverses ADR 0016's cold path), and
///   a mutating case's per-case vault isolation is unchanged — but a read
///   case is not per-case: read cases (`mutating: false`) share ONE cached
///   vault per (fixture, side) (`crate::fixtures`), and therefore share that
///   cached vault's summoned owner too. Only the mutating cases get a fresh
///   vault, and their own owner, per case. Before this override, the
///   worst-case lingering population was on the order of the mutating-case
///   count plus the number of distinct read fixtures, candidate-side only —
///   roughly 75 owners (67 mutating cases + ~8 distinct read fixtures), not
///   one per case. Left at the 120s production default they would linger
///   that long after the run exits; the short override makes each one
///   self-reap promptly behind the run instead.
///   The pinned 0.48.x oracle has no ephemeral-owner tier and no reader for
///   this env var — only the candidate side ever summons an owner — which is
///   why forcing the same value on both sides does not turn the host into an
///   input despite the "same value is not enough" rule above: the oracle
///   deterministically ignores it. Accepted trade-off: at a short TTL,
///   consecutive same-fixture read cases spaced more than TTL-seconds apart
///   pay a ~30ms re-summon instead of reusing the still-warm owner. The same
///   TTL also caps the OTHER side of that trade-off — the lingering-owner
///   ceiling — at roughly TTL ÷ per-case wall time concurrently-lingering
///   owners. Assuming a ~100ms per-case wall time: order-50 at the current
///   5s TTL, order-20 at a 2s TTL, against the old 120s production
///   default's order-1200 by the same formula — a ceiling never actually
///   reached because a full gated run finishes well under 120s, so at that
///   default owners simply accumulate for the whole run instead of
///   self-reaping.
pub struct SpawnEnv {
    home: PathBuf,
    cache: PathBuf,
    config: PathBuf,
    /// Owned so the sockets and logs a daemon-capable binary opens under it
    /// are removed when the run ends.
    runtime: tempfile::TempDir,
}

/// The env var a `norn` binary reads to override its summoned-owner idle TTL
/// (`norn_client::EPHEMERAL_TTL_ENV`). Duplicated as a literal rather than
/// imported — the parity harness spawns `norn` as an opaque subprocess and
/// must not link the crate it exercises, matching this file's existing
/// `NORN_ROOT` / `NORN_CONFIG_DIR` literals.
const EPHEMERAL_TTL_ENV: &str = "NORN_EPHEMERAL_TTL_SECS";

/// How long a parity-spawned owner lingers idle before self-reaping. Short
/// relative to the 120s production default (see [`SpawnEnv`]'s doc); long
/// enough that a case's own request sequence against its fixture vault
/// finishes well inside it. `5`, not `2`, matching the sibling convention
/// (`TEST_OWNER_TTL_SECS` in `crates/norn/tests/cli.rs`) — a 2s TTL races a
/// loaded CI runner's scheduling stalls (see
/// `crates/norn-owner/tests/get_markdown_selection_guard.rs`'s NRN-462
/// comment); 5s still fully solves lingering.
const OWNER_IDLE_TTL_SECS: &str = "5";

impl SpawnEnv {
    /// Create the scratch `HOME` / XDG tree under `root` — a directory the
    /// caller owns for the run (the fixture cache's temp root in a
    /// comparison run).
    pub fn create_in(root: &Path) -> std::io::Result<SpawnEnv> {
        let home = root.join("scratch-home");
        let cache = root.join("scratch-cache");
        let config = root.join("scratch-config");
        // Created, not just named: an XDG consumer may assume its base
        // directory exists and not create parents, and a write that fails for
        // that reason reads as the binary's behavior rather than as setup.
        for dir in [&home, &cache, &config] {
            std::fs::create_dir_all(dir)?;
        }
        let runtime = norn_fixtures::testing::short_runtime_dir("norn-parity-")?;
        Ok(SpawnEnv {
            home,
            cache,
            config,
            runtime,
        })
    }

    fn apply(&self, command: &mut Command) {
        command.env_clear();
        // PATH so a binary can find anything it shells out to, and TMPDIR
        // because a process needs somewhere to write. Nothing else is
        // forwarded — see the type doc.
        for passthrough in ["PATH", "TMPDIR"] {
            if let Some(value) = std::env::var_os(passthrough) {
                command.env(passthrough, value);
            }
        }
        command.env("LC_ALL", "C.UTF-8");
        command.env_remove("LANG");
        command.env_remove("LC_CTYPE");
        command.env("HOME", &self.home);
        command.env("XDG_CACHE_HOME", &self.cache);
        command.env("XDG_CONFIG_HOME", &self.config);
        command.env("XDG_RUNTIME_DIR", self.runtime.path());
        command.env_remove("NORN_ROOT");
        command.env_remove("NORN_CONFIG_DIR");
        command.env(EPHEMERAL_TTL_ENV, OWNER_IDLE_TTL_SECS);
    }
}

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

/// Retry `attempt` while it fails with `ExecutableFileBusy` (ETXTBSY), up to
/// 25 attempts spaced 20ms apart — a bound of roughly 480ms before the error
/// is returned to the caller.
///
/// `execve` refuses to run an image that is open for writing anywhere on the
/// system: it takes a write-deny reference on the inode, which fails while
/// any file description still holds a write reference to it. A binary this
/// process wrote moments ago can satisfy that even after its own descriptor
/// is closed — another thread forking inside the write's window gives the
/// child a copy of the parent's file descriptor table, and that inherited
/// description keeps the inode's writer count above zero until the child
/// execs. Close-on-exec does clear it, but the kernel takes the write-deny
/// reference for the NEW image before flushing the old table, so the window
/// is real. It is also transient, lifting as soon as that descriptor closes,
/// so spawning waits it out rather than failing the run. A spawn that fails
/// never started a process, so a retry repeats no side effect.
fn retry_while_busy<T>(mut attempt: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    const MAX_ATTEMPTS: u32 = 25;
    const BACKOFF: Duration = Duration::from_millis(20);

    for _ in 1..MAX_ATTEMPTS {
        match attempt() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(BACKOFF);
            }
            other => return other,
        }
    }
    attempt()
}

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
    env: &SpawnEnv,
    binary_label: &str,
) -> Result<Spawned, ExecError> {
    let mut child = retry_while_busy(|| {
        let mut command = Command::new(binary);
        env.apply(&mut command);
        command
            .args(argv)
            .current_dir(vault)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    })
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
pub fn run_case(
    binary: &Path,
    case: &Case,
    vault: &Path,
    env: &SpawnEnv,
) -> Result<RawOutput, ExecError> {
    debug_assert!(
        case.stdin.is_none(),
        "an MCP case (stdin: Some) must be driven by crate::mcp::run_case, not exec::run_case"
    );
    run_argv(binary, case.argv, None, vault, env)
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
    env: &SpawnEnv,
) -> Result<RawOutput, ExecError> {
    let binary_label = binary.display().to_string();
    let Spawned {
        child,
        stdin_writer,
    } = spawn_with_stdin(binary, argv, stdin, vault, env, &binary_label)?;

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
    env: &SpawnEnv,
    timeout: Duration,
) -> Result<RawOutput, ExecError> {
    const POLL_INTERVAL: Duration = Duration::from_millis(20);

    let binary_label = binary.display().to_string();
    let Spawned {
        mut child,
        stdin_writer,
    } = spawn_with_stdin(binary, argv, stdin, vault, env, &binary_label)?;

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
pub fn probe_version(binary: &Path, env: &SpawnEnv) -> Result<RawOutput, ExecError> {
    let binary_label = binary.display().to_string();
    let output = retry_while_busy(|| {
        let mut command = Command::new(binary);
        env.apply(&mut command);
        command.arg("--version").stdin(Stdio::null()).output()
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn busy() -> std::io::Error {
        std::io::Error::from(std::io::ErrorKind::ExecutableFileBusy)
    }

    #[test]
    fn a_transient_busy_is_waited_out() {
        let attempts = Cell::new(0);
        let result = retry_while_busy(|| {
            attempts.set(attempts.get() + 1);
            if attempts.get() < 3 {
                Err(busy())
            } else {
                Ok("spawned")
            }
        });
        assert_eq!(result.unwrap(), "spawned");
        assert_eq!(attempts.get(), 3, "retried exactly until it succeeded");
    }

    #[test]
    fn a_permanent_busy_gives_up_after_the_attempt_budget() {
        let attempts = Cell::new(0);
        let result: std::io::Result<()> = retry_while_busy(|| {
            attempts.set(attempts.get() + 1);
            Err(busy())
        });
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::ExecutableFileBusy,
            "the last error reaches the caller rather than being swallowed"
        );
        assert_eq!(
            attempts.get(),
            25,
            "the budget is 25 attempts, not unbounded"
        );
    }

    #[test]
    fn any_other_error_is_returned_on_the_first_attempt() {
        let attempts = Cell::new(0);
        let result: std::io::Result<()> = retry_while_busy(|| {
            attempts.set(attempts.get() + 1);
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no such binary",
            ))
        });
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(err.to_string(), "no such binary", "propagated verbatim");
        assert_eq!(attempts.get(), 1, "only ETXTBSY is worth waiting out");
    }
}
