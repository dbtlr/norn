//! `norn serve` — the warm host daemon.
//!
//! One persistent foreground process (Unix only) that serves the full MCP
//! toolset for any vault on this host over a single well-known Unix-domain
//! socket, holding lazy per-vault verify-once warm caches. This module is the
//! entry point: it establishes single ownership, binds the socket, and runs the
//! accept loop; the per-connection protocol lives in [`conn`], the per-vault
//! context map in [`contexts`], and the on-disk lifecycle primitives in
//! [`lifecycle`].
//!
//! The design follows ADR 0005 (+ its 2026-07-04 amendment):
//!
//! - **Single ownership via a lifetime flock.** The daemon holds an exclusive
//!   advisory lock next to the socket for its whole life, so a live handshake
//!   proves exactly one authoritative warm cache is serving — never a race of N
//!   daemons at the same socket.
//! - **O(1) ping off the accept loop.** Every connection is a fresh task. A
//!   host-global routing ping touches no vault and takes no map lock; a scoped
//!   status ping performs one bounded map lookup and coherent progress snapshot
//!   without opening the vault or touching its filesystem. Long vault work runs
//!   on `spawn_blocking` threads (see `mcp::server`), keeping async workers free.
//! - **Verify-once per vault.** Each vault's warm [`crate::env::VaultEnv`]
//!   checks integrity once, then self-heals per request — so warm queries skip
//!   the repeated integrity check the one-shot CLI pays.

#[cfg(unix)]
pub(crate) mod conn;
#[cfg(unix)]
pub(crate) mod contexts;
#[cfg(unix)]
pub(crate) mod heal;
#[cfg(unix)]
pub(crate) mod lifecycle;

#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use camino::Utf8Path;

/// Run the warm host daemon. Blocks until SIGINT/SIGTERM, then unlinks the
/// socket and returns `Ok(())` (exit 0). Returns `Err` on a startup failure
/// (run-dir, lock contention, bind), which the CLI renders and exits non-zero.
#[cfg(unix)]
pub fn run() -> anyhow::Result<()> {
    // Lifecycle, in order (ADR 0005): run dir → single-owner flock → socket.
    let run_dir = crate::service::run_dir()?;
    lifecycle::ensure_run_dir(&run_dir)?;

    let lock_path = crate::service::host_lock_path()?;
    // Held for the whole process lifetime — dropping it releases single
    // ownership, so it must outlive the runtime below.
    let _lock = lifecycle::acquire_host_lock(&lock_path)?;

    let socket_path = crate::service::host_socket_path()?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        // Register the shutdown signal handlers BEFORE binding the listener:
        // if registration fails, we bail out with no socket left bound for a
        // future probe to mistake for a live daemon.
        use tokio::signal::unix::{signal, SignalKind};
        let sigint = signal(SignalKind::interrupt())?;
        let sigterm = signal(SignalKind::terminate())?;

        // Install the fail-closed self-heal trigger BEFORE serving so a poisoned
        // state surfaced by the very first request can still route an exit
        // (NRN-337).
        let poison = heal::install();

        let listener = lifecycle::bind_listener(&socket_path)?;
        eprintln!(
            "norn serve: listening at {socket_path} (v{}, pid {})",
            env!("CARGO_PKG_VERSION"),
            std::process::id()
        );
        serve_loop(listener, &socket_path, sigint, sigterm, poison).await
    })
}

/// Non-Unix stub: the daemon rides Unix-domain sockets, so it cannot run here.
#[cfg(not(unix))]
pub fn run() -> anyhow::Result<()> {
    anyhow::bail!("norn serve requires a Unix host (unix-domain sockets)")
}

/// Accept connections until a shutdown signal, spawning a task per connection.
/// On SIGINT/SIGTERM: stop accepting, unlink the socket, return (exit 0).
/// In-flight connections may be dropped — the client falls back to Direct.
///
/// `sigint`/`sigterm` are registered by the caller (before the listener is
/// bound — see [`run`]) so a signal-registration failure never leaves the
/// socket bound with nothing watching it.
#[cfg(unix)]
async fn serve_loop(
    listener: tokio::net::UnixListener,
    socket_path: &Utf8Path,
    mut sigint: tokio::signal::unix::Signal,
    mut sigterm: tokio::signal::unix::Signal,
    poison: Arc<tokio::sync::Notify>,
) -> anyhow::Result<()> {
    let contexts = Arc::new(contexts::Contexts::new());
    let start = Instant::now();

    // Set when we break because of a poisoned state (NRN-337) rather than a
    // shutdown signal, so we grant in-flight responses a brief flush window
    // before the runtime tears them down.
    let mut poisoned = false;

    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _addr)) => {
                    let contexts = Arc::clone(&contexts);
                    tokio::spawn(async move {
                        if let Err(e) = conn::handle_connection(stream, contexts, start).await {
                            eprintln!("norn serve: connection error: {e}");
                        }
                    });
                }
                // fd exhaustion at accept is the wedge NRN-337 targets: the fd
                // table is full, so every subsequent accept/open on every vault
                // fails. Exit to self-heal rather than hot-spinning EMFILE
                // forever; the next invocation respawns a fresh daemon.
                Err(e) if heal::is_fd_exhaustion_io(&e) => {
                    heal::trip("file-descriptor exhaustion at accept (EMFILE/ENFILE)");
                    poisoned = true;
                    break;
                }
                Err(e) => {
                    eprintln!("norn serve: accept error: {e}");
                    // Back off briefly so a persistent (non-fd) accept error
                    // doesn't hot-spin the loop at 100% CPU. With the sleep the
                    // worst case is ~10 log lines/sec — acceptable.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            _ = sigint.recv() => break,
            _ = sigterm.recv() => break,
            // A request path (or the accept arm above) hit a poisoned state and
            // tripped the latch: stop accepting and exit to self-heal (NRN-337).
            _ = poison.notified() => {
                poisoned = true;
                break;
            }
        }
    }

    if poisoned {
        // Let the request whose error tripped the latch serialize and write its
        // response before the runtime drops in-flight connection tasks. Only on
        // the poison path — a signal shutdown needs no grace.
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    // Graceful shutdown: unlink the socket so the next probe sees no daemon.
    let _ = std::fs::remove_file(socket_path.as_std_path());
    eprintln!("norn serve: shutting down");
    Ok(())
}
