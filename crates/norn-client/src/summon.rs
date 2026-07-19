//! Spawning the owner (ADR 0017): run the owner executable in owner mode,
//! detached into its own process group so it outlives the summoning CLI.
//!
//! The client is the ONLY crate that spawns owners. The owner-mode argv is an
//! internal contract (mirrored in `norn-owner`): `<exe> __norn-owner --socket
//! <path> --vault-root <path> --ttl-secs <n> --build <fp>`.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::error::ClientError;

/// The owner-mode sentinel — must equal `norn_owner::OWNER_MODE_ARG`. Duplicated
/// (not imported) because the client must NOT depend on `norn-owner` (the
/// crate-map keeps "the client never opens a cache" compile-enforced); this
/// literal is the wire between them, pinned by a test in `norn-owner`.
pub const OWNER_MODE_ARG: &str = "__norn-owner";

/// Spawn `owner_exe` in owner mode, detached, told to bind `socket` for
/// `vault_root` with the given idle TTL and build fingerprint. Returns as soon
/// as the process is launched — the caller then connects with bounded retry.
pub fn spawn_owner(
    owner_exe: &Path,
    socket: &Path,
    vault_root: &Path,
    idle_ttl: Duration,
    build: &str,
) -> Result<(), ClientError> {
    let mut command = Command::new(owner_exe);
    command
        .arg(OWNER_MODE_ARG)
        .arg("--socket")
        .arg(socket)
        .arg("--vault-root")
        .arg(vault_root)
        .arg("--ttl-secs")
        .arg(idle_ttl.as_secs().to_string())
        .arg("--build")
        .arg(build)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Keep the owner's stderr visible: warm-up / exit-to-heal diagnostics go
        // there, and a detached daemon's stderr is the only operator signal.
        .stderr(Stdio::inherit());

    // Detach from the CLI's process group so terminal job-control signals to the
    // foreground CLI (SIGINT on ^C, SIGHUP on terminal close) don't also reap
    // the owner. `process_group(0)` is the safe std detach — no `setsid` unsafe.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    // Spawn and immediately drop the `Child` handle: the owner is a detached
    // daemon, not a child we wait on. Not reaping it risks a zombie only if it
    // exits while we still run — but a loser-of-the-flock owner exits in
    // milliseconds and the CLI itself is short-lived, so the OS reaps it on our
    // exit. (A future resident tier that supervises owners would hold the handle.)
    command
        .spawn()
        .map(|_child| ())
        .map_err(|source| ClientError::Spawn {
            exe: owner_exe.to_path_buf(),
            source,
        })
}
