//! The per-vault mutation lock every mutating surface acquires.
//!
//! A single advisory flock, next to the vault's cache, that serializes
//! mutations across the CLI, the MCP server, and the daemon so two writers can
//! never race one vault. Callers acquire it for the duration of an apply and
//! get a typed `CacheError::MutationLockTimeout` on contention, which the
//! refusal seams turn into a `mutation-lock-timeout` code. `pending` sweeps
//! stale pending markers before acquire.

pub mod pending;

use crate::cache::{acquire_flock, CacheError};
use camino::Utf8Path;
use std::time::Duration;

pub const MUTATION_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// Test-only override for the mutation-lock acquire timeout, in milliseconds.
/// Lets contention tests hit `CacheError::MutationLockTimeout` in ~100–200ms
/// instead of stalling the suite the full 5s per contended acquire. DEBUG
/// BUILDS ONLY: the env read is compiled out of release builds (see
/// [`mutation_lock_timeout`]), so an inherited value in a production
/// environment (a dev shell, a baked plist env) can never shrink the real 5s
/// budget. Same pattern as the cache write lock's `NORN_CACHE_LOCK_TIMEOUT_MS`
/// (src/cache/lock.rs).
#[cfg(debug_assertions)]
const MUTATION_LOCK_TIMEOUT_ENV: &str = "NORN_MUTATION_LOCK_TIMEOUT_MS";

/// The mutation-lock acquire timeout used by every mutating command surface.
///
/// **Release builds always return [`MUTATION_LOCK_TIMEOUT`] (5s)** — the env
/// read below is `#[cfg(debug_assertions)]`, compiled out entirely, so no
/// environment can alter the production timeout. Debug builds (what
/// `cargo test` builds and what its integration tests spawn) honor
/// `NORN_MUTATION_LOCK_TIMEOUT_MS` when it parses to a positive integer, read
/// at every acquire (not cached) so a test child process scopes the override
/// to its own environment.
fn mutation_lock_timeout() -> Duration {
    #[cfg(debug_assertions)]
    if let Ok(raw) = std::env::var(MUTATION_LOCK_TIMEOUT_ENV) {
        if let Ok(ms) = raw.parse::<u64>() {
            if ms > 0 {
                return Duration::from_millis(ms);
            }
        }
    }
    MUTATION_LOCK_TIMEOUT
}

#[derive(Debug)]
pub struct MutationLock {
    _file: std::fs::File,
}

impl MutationLock {
    pub fn acquire_if_mutating(
        state_dir: &Utf8Path,
        is_apply: bool,
    ) -> Result<Option<Self>, CacheError> {
        Self::acquire_with_timeout(state_dir, is_apply, mutation_lock_timeout())
    }

    fn acquire_with_timeout(
        state_dir: &Utf8Path,
        is_apply: bool,
        timeout: Duration,
    ) -> Result<Option<Self>, CacheError> {
        if !is_apply {
            return Ok(None);
        }
        // Ensure state dir exists.
        std::fs::create_dir_all(state_dir.as_std_path()).map_err(|e| {
            CacheError::MutationLockIo {
                path: state_dir.to_owned(),
                source: e,
            }
        })?;
        let lock_path = state_dir.join(".mutation.lock");
        acquire_flock(&lock_path, timeout)
            .map(|f| Some(MutationLock { _file: f }))
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    CacheError::MutationLockTimeout
                } else {
                    CacheError::MutationLockIo {
                        path: lock_path,
                        source: e,
                    }
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    #[test]
    fn no_lock_when_dry_run() {
        let tmp = TempDir::new().unwrap();
        let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let result = MutationLock::acquire_if_mutating(&dir, false).unwrap();
        assert!(result.is_none(), "dry-run must not acquire a lock");
        // Lock file should NOT have been created.
        assert!(!tmp.path().join(".mutation.lock").exists());
    }

    #[test]
    fn acquires_lock_when_apply() {
        let tmp = TempDir::new().unwrap();
        let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let guard = MutationLock::acquire_if_mutating(&dir, true).unwrap();
        assert!(guard.is_some());
        assert!(tmp.path().join(".mutation.lock").exists());
    }

    #[test]
    fn second_caller_gets_mutation_lock_timeout_error() {
        let tmp = TempDir::new().unwrap();
        let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        // Hold the lock file directly using fs2 to simulate a concurrent mutation.
        std::fs::create_dir_all(tmp.path()).unwrap();
        let lock_path = dir.join(".mutation.lock");
        use fs2::FileExt;
        let held = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path.as_std_path())
            .unwrap();
        held.try_lock_exclusive().unwrap();

        // Call acquire_with_timeout with a short timeout — must return MutationLockTimeout.
        let result = MutationLock::acquire_with_timeout(&dir, true, Duration::from_millis(150));
        assert!(
            matches!(result, Err(CacheError::MutationLockTimeout)),
            "expected MutationLockTimeout, got: {result:?}"
        );
        drop(held);
    }
}
