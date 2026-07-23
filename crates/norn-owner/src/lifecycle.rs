//! Owner lifecycle primitives: runtime dir, single-owner flock, socket bind.
//!
//! Targets a per-vault ephemeral owner rather than one host daemon (ADR 0017):
//!
//! - The advisory flock is the load-bearing single-owner primitive (ADR 0005,
//!   carried forward by 0017): the owner holds it for its whole life, so a live
//!   connection to the socket means exactly one authoritative warm cache is
//!   serving this vault+build — never a race of N owners at one socket. When two
//!   clients summon simultaneously, both spawn owners; the flock serializes them
//!   — the loser fails to acquire it and exits WITHOUT touching the socket
//!   (order below is flock-then-bind precisely so the loser never clobbers the
//!   winner's bound socket), and its client connects to the winner instead.
//! - Each function is parameterized by path so tests exercise them against temp
//!   paths; the runtime entry wires the real per-vault socket path.

use anyhow::Context as _;
use camino::Utf8Path;

/// Ensure the runtime dir exists with mode 0700 (owner-only) — it holds the
/// per-vault control socket and its advisory lock. Idempotent: the summoner
/// creates it at first summon, and the owner re-ensures it defensively.
///
/// Security (finding 5): after creating, `lstat` the dir and REJECT it if it is
/// a symlink — the classic pre-creation attack on the world-writable
/// `$TMPDIR/norn-<uid>` fallback is to plant a symlink the owner would then
/// adopt and bind its socket through. (The summoning client, which creates the
/// dir first and runs as the same uid, additionally checks ownership.)
pub fn ensure_runtime_dir(runtime_dir: &Utf8Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(runtime_dir.as_std_path())
        .with_context(|| format!("failed to create runtime dir {runtime_dir}"))?;

    let meta = std::fs::symlink_metadata(runtime_dir.as_std_path())
        .with_context(|| format!("failed to lstat runtime dir {runtime_dir}"))?;
    if meta.file_type().is_symlink() {
        anyhow::bail!("refusing runtime dir that is a symlink: {runtime_dir}");
    }

    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            runtime_dir.as_std_path(),
            std::fs::Permissions::from_mode(0o700),
        )
        .with_context(|| format!("failed to set 0700 on runtime dir {runtime_dir}"))?;
    }
    Ok(())
}

/// The result of trying to acquire the single-owner lock.
pub enum AcquireOutcome {
    /// This process won single ownership. Hold the `File` for the process
    /// lifetime (dropping it releases the lock).
    Acquired(std::fs::File),
    /// Another owner already serves this vault+build. This is NORMAL — losing a
    /// concurrent summon race is expected — so the caller steps aside cleanly
    /// (exit 0) and its client connects to the incumbent instead. Carries the
    /// incumbent's pid when readable, for the info log line.
    Contended { incumbent_pid: Option<String> },
}

/// Try to acquire the single-owner advisory lock at `lock_path`.
///
/// On success, truncates the file, writes `<pid> <version>\n`, and returns
/// [`AcquireOutcome::Acquired`]. On contention returns
/// [`AcquireOutcome::Contended`] — NOT an error (losing the summon race is
/// normal). Only a genuine filesystem failure (flock unsupported, EIO, …) is an
/// `Err`.
pub fn acquire_owner_lock(lock_path: &Utf8Path, version: &str) -> anyhow::Result<AcquireOutcome> {
    use fs2::FileExt;
    use std::io::{Read, Seek, Write};

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path.as_std_path())
        .with_context(|| format!("failed to open owner lock {lock_path}"))?;

    match file.try_lock_exclusive() {
        Ok(()) => {}
        // Contention — the incumbent owner holds it. fs2 signals a contended
        // lock with `ErrorKind::WouldBlock`. Not an error: report the incumbent
        // pid so the caller can log stepping aside at info.
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            let mut body = String::new();
            let _ = file.read_to_string(&mut body);
            let incumbent_pid = body
                .split_whitespace()
                .next()
                .filter(|pid| !pid.is_empty())
                .map(str::to_string);
            return Ok(AcquireOutcome::Contended { incumbent_pid });
        }
        // Any OTHER error (an flock-unsupported filesystem, EIO, …) is NOT
        // contention; propagate it so the real failure is debuggable.
        Err(e) => {
            return Err(
                anyhow::Error::new(e).context(format!("failed to lock owner lock {lock_path}"))
            );
        }
    }

    file.set_len(0)?;
    file.seek(std::io::SeekFrom::Start(0))?;
    writeln!(file, "{} {}", std::process::id(), version)?;
    file.flush()?;
    Ok(AcquireOutcome::Acquired(file))
}

/// Bind the control socket at `socket_path`, reclaiming a stale file first.
///
/// The owner holds the flock by the time this runs, so any socket file present
/// is ours to reclaim (a previous owner that died without unlinking). Must be
/// called from within a tokio runtime.
pub fn bind_listener(socket_path: &Utf8Path) -> anyhow::Result<tokio::net::UnixListener> {
    if socket_path.as_std_path().exists() {
        let _ = std::fs::remove_file(socket_path.as_std_path());
    }
    let listener = tokio::net::UnixListener::bind(socket_path.as_std_path())
        .with_context(|| format!("failed to bind control socket {socket_path}"))?;
    // Owner-only socket mode; `bind` follows the ambient umask, so pin 0600.
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            socket_path.as_std_path(),
            std::fs::Permissions::from_mode(0o600),
        );
    }
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    #[test]
    fn contended_lock_reports_contended_with_pid() {
        use fs2::FileExt;
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = Utf8PathBuf::from_path_buf(tmp.path().join("owner.lock")).unwrap();

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

        // Losing the race is NORMAL — a Contended outcome, not an error.
        match acquire_owner_lock(&lock_path, "0.0.0").expect("contention is not an error") {
            AcquireOutcome::Contended { incumbent_pid } => {
                assert_eq!(incumbent_pid.as_deref(), Some("424242"));
            }
            AcquireOutcome::Acquired(_) => panic!("should not acquire a held lock"),
        }
        drop(held);
    }

    #[test]
    fn free_lock_is_acquired_and_stamped() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = Utf8PathBuf::from_path_buf(tmp.path().join("owner.lock")).unwrap();
        let outcome = acquire_owner_lock(&lock_path, "9.9.9").expect("free lock should acquire");
        let file = match outcome {
            AcquireOutcome::Acquired(file) => file,
            AcquireOutcome::Contended { .. } => panic!("a free lock must acquire"),
        };
        let body = std::fs::read_to_string(lock_path.as_std_path()).unwrap();
        assert!(
            body.starts_with(&std::process::id().to_string()),
            "body: {body:?}"
        );
        assert!(body.contains("9.9.9"));
        drop(file);
    }

    #[test]
    fn runtime_dir_is_created_0700() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8PathBuf::from_path_buf(tmp.path().join("norn-rt")).unwrap();
        ensure_runtime_dir(&dir).unwrap();
        let mode = std::fs::metadata(dir.as_std_path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "runtime dir must be owner-only");
    }

    #[test]
    fn runtime_dir_that_is_a_symlink_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        // A real target dir plus a symlink pointing at it — the pre-creation
        // attack shape. Adopting the symlink must be refused.
        let target = tmp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = tmp.path().join("norn-rt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let link_utf8 = Utf8PathBuf::from_path_buf(link).unwrap();
        let err =
            ensure_runtime_dir(&link_utf8).expect_err("a symlinked runtime dir must be rejected");
        assert!(
            err.to_string().contains("symlink"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn stale_socket_is_reclaimed_on_bind() {
        let tmp = tempfile::tempdir().unwrap();
        let socket_path = Utf8PathBuf::from_path_buf(tmp.path().join("v.sock")).unwrap();
        let stale = std::os::unix::net::UnixListener::bind(socket_path.as_std_path()).unwrap();
        drop(stale);
        assert!(socket_path.as_std_path().exists());
        let listener = bind_listener(&socket_path).expect("should reclaim + bind");
        drop(listener);
    }
}
