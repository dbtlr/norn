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
    config_override: Option<&Path>,
    events_dir: Option<&Path>,
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
        .arg(build);
    // The resolver-derived `[vaults.<name>].config` override (ADR 0017), passed
    // through only when the registry supplied one; otherwise the owner defaults
    // to `<vault_root>/.norn/config.yaml`.
    if let Some(config) = config_override {
        command.arg("--config").arg(config);
    }
    // The durable telemetry events dir (NRN-400), passed only for a registered
    // vault; its absence tells the owner to keep in-memory (ephemeral) telemetry.
    if let Some(events) = events_dir {
        command.arg("--events-dir").arg(events);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Route the owner's stderr to a per-owner log file (truncate-on-start)
        // rather than inheriting the CLI's stderr fd — a detached owner holding
        // the CLI's stderr open for up to a full TTL would keep a piped stderr
        // from ever closing (finding 4). The log preserves operator visibility
        // (warm-up / exit-to-heal / lost-summon-race diagnostics land there).
        // Best-effort: if the log can't be opened, discard rather than inherit,
        // so the fd stays decoupled regardless.
        .stderr(owner_log_stderr(socket));

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

/// The owner's stderr target: a per-owner log file `<h>.<fp>.log` beside the
/// socket, truncated on start. Falls back to a null sink (never the client's
/// inherited stderr) so the fd is always decoupled.
fn owner_log_stderr(socket: &Path) -> Stdio {
    let log_path = socket.with_extension("log");
    match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)
    {
        Ok(file) => Stdio::from(file),
        Err(_) => Stdio::null(),
    }
}
