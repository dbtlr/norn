//! Daemon lifecycle primitives: run dir, single-owner flock, socket bind.
//!
//! Each function is parameterized by path so the unit tests can exercise them
//! against temp paths; only the top-level entry ([`super::run`]) wires the real
//! `<XDG_CACHE_HOME>/norn/run/` paths. This keeps the tests hermetic without
//! `std::env::set_var` games.
//!
//! The advisory flock is the load-bearing trust primitive (ADR 0005): the daemon
//! is the *sole routed owner* of the well-known socket for as long as it holds
//! the lock. Holding it for the whole process lifetime is what lets a client
//! trust that a live handshake means one authoritative warm cache, not one of N
//! racing daemons.

use anyhow::Context as _;
use camino::Utf8Path;

/// Ensure the run directory exists with mode 0700 (owner-only) — it holds the
/// control socket and the advisory lock. Mirrors the cache dir's security
/// posture (`mcp::context::ensure_cache_dir`).
pub(crate) fn ensure_run_dir(run_dir: &Utf8Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(run_dir.as_std_path())
        .with_context(|| format!("failed to create run dir {run_dir}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            run_dir.as_std_path(),
            std::fs::Permissions::from_mode(0o700),
        )
        .with_context(|| format!("failed to set 0700 on run dir {run_dir}"))?;
    }
    Ok(())
}

/// Acquire the single-owner advisory lock at `lock_path`.
///
/// On success, truncates the file and writes `<pid> <version>\n`, then returns
/// the open `File` — the caller MUST hold it for the process lifetime (dropping
/// it releases the lock). On contention, reads the incumbent's pid from the
/// lockfile body and returns an "already running" error.
pub(crate) fn acquire_host_lock(lock_path: &Utf8Path) -> anyhow::Result<std::fs::File> {
    use fs2::FileExt;
    use std::io::{Read, Seek, Write};

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path.as_std_path())
        .with_context(|| format!("failed to open host lock {lock_path}"))?;

    match file.try_lock_exclusive() {
        Ok(()) => {}
        // Contention — the incumbent holds it. fs2 signals a contended lock with
        // `ErrorKind::WouldBlock` (its `lock_contended_error()`); report the
        // incumbent's pid if we can read it. (FIX-9)
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            let mut body = String::new();
            let _ = file.read_to_string(&mut body);
            match body.split_whitespace().next() {
                Some(pid) if !pid.is_empty() => {
                    anyhow::bail!("norn serve: another instance is already running (pid {pid})")
                }
                _ => anyhow::bail!("norn serve: another instance is already running"),
            }
        }
        // Any OTHER error (NFS/flock-unsupported filesystem, EIO, …) is NOT
        // contention; propagate it so the real failure is debuggable rather than
        // masquerading as "already running". (FIX-9)
        Err(e) => {
            return Err(
                anyhow::Error::new(e).context(format!("failed to lock host lock {lock_path}"))
            );
        }
    }

    // Acquired. Record our pid + version for diagnostics and for the contention
    // message the next `norn serve` would print.
    file.set_len(0)?;
    file.seek(std::io::SeekFrom::Start(0))?;
    writeln!(file, "{} {}", std::process::id(), env!("CARGO_PKG_VERSION"))?;
    file.flush()?;
    Ok(file)
}

/// Bind the control socket at `socket_path`, reclaiming a stale file first.
///
/// We hold the host flock by the time this runs, so any socket file present is
/// ours to reclaim (a previous instance that died without unlinking). Must be
/// called from within a tokio runtime (`UnixListener::bind` registers with the
/// reactor).
pub(crate) fn bind_listener(socket_path: &Utf8Path) -> anyhow::Result<tokio::net::UnixListener> {
    if socket_path.as_std_path().exists() {
        let _ = std::fs::remove_file(socket_path.as_std_path());
    }
    tokio::net::UnixListener::bind(socket_path.as_std_path())
        .with_context(|| format!("failed to bind control socket {socket_path}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    /// With the lock file already held (simulating a running incumbent), the
    /// acquire fn returns the "already running" error rather than succeeding.
    #[test]
    fn contended_lock_reports_already_running() {
        use fs2::FileExt;
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = Utf8PathBuf::from_path_buf(tmp.path().join("norn.lock")).unwrap();

        // Hold the lock like an incumbent daemon would, with a pid in the body.
        let held = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path.as_std_path())
            .unwrap();
        held.try_lock_exclusive().unwrap();
        {
            use std::io::Write;
            let mut w = &held;
            writeln!(w, "424242 0.0.0").unwrap();
            w.flush().unwrap();
        }

        let err = acquire_host_lock(&lock_path).expect_err("must refuse a contended lock");
        assert!(
            err.to_string().contains("already running"),
            "unexpected error: {err}"
        );
        assert!(
            err.to_string().contains("424242"),
            "should surface the incumbent pid: {err}"
        );
        drop(held);
    }

    /// A free lock is acquired and stamped with our pid + version.
    #[test]
    fn free_lock_is_acquired_and_stamped() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = Utf8PathBuf::from_path_buf(tmp.path().join("norn.lock")).unwrap();
        let file = acquire_host_lock(&lock_path).expect("free lock should acquire");
        let body = std::fs::read_to_string(lock_path.as_std_path()).unwrap();
        assert!(
            body.starts_with(&std::process::id().to_string()),
            "lock body should start with our pid, got {body:?}"
        );
        drop(file);
    }

    /// A stale socket file at the path is unlinked and the listener binds.
    #[tokio::test]
    async fn stale_socket_is_reclaimed_on_bind() {
        let tmp = tempfile::tempdir().unwrap();
        let socket_path = Utf8PathBuf::from_path_buf(tmp.path().join("norn.sock")).unwrap();

        // Leave a stale socket file behind (std UnixListener does not unlink on
        // drop), then prove bind_listener reclaims it.
        let stale = std::os::unix::net::UnixListener::bind(socket_path.as_std_path()).unwrap();
        drop(stale);
        assert!(
            socket_path.as_std_path().exists(),
            "stale socket should exist"
        );

        let listener = bind_listener(&socket_path).expect("should reclaim + bind");
        drop(listener);
    }
}
