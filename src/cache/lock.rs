//! Advisory file lock for serializing cache write operations.
//!
//! Acquires an exclusive `flock(2)` (via fs2) on `<cache_dir>/.lock`.
//! Reads never block; only `Cache::rebuild` / `Cache::index_incremental`
//! (and other write paths) take this lock. WAL mode (set at open time)
//! is what makes concurrent reads safe alongside an in-flight write.

use camino::Utf8Path;
use fs2::FileExt;

use crate::cache::error::CacheError;

/// The production write-lock acquire timeout for the cache write paths
/// (`Cache::rebuild` / `Cache::index_incremental`): 5 seconds, always, unless
/// the test-only env override below is set.
pub(crate) const DEFAULT_WRITE_LOCK_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(5);

/// Test-only override for the write-lock acquire timeout, in milliseconds.
/// Lets contention tests hit `CacheError::LockTimeout` in ~100–200ms instead
/// of stalling the suite the full 5s per contended acquire. DEBUG BUILDS ONLY:
/// the env read is compiled out of release builds (see
/// [`debug_env_duration_ms`]), so an inherited value in a production environment
/// (a dev shell, a baked plist env) can never shrink the real 5s budget.
const WRITE_LOCK_TIMEOUT_ENV: &str = "NORN_CACHE_LOCK_TIMEOUT_MS";

/// Shared debug-only `env → Duration` override reader for the cache write paths.
///
/// **Release builds always return `default`** — the env read is
/// `#[cfg(debug_assertions)]`, compiled out entirely, so no environment can alter
/// a production timeout/budget. Debug builds (what `cargo test` builds and its
/// integration tests spawn) honor `var` when it parses to an integer, read at
/// every call (not cached) so a test child process scopes the override to itself.
///
/// `accept_zero` parameterizes the ONE behavioral difference between the two call
/// sites: the write-lock timeout REJECTS `0` (a zero-length acquire budget is
/// nonsensical — fall back to the real 5s), while the increment-chunk budget
/// ACCEPTS `0` (0ms deliberately forces one-file-per-chunk in tests).
#[cfg_attr(not(debug_assertions), allow(unused_variables))]
pub(crate) fn debug_env_duration_ms(
    var: &str,
    default: std::time::Duration,
    accept_zero: bool,
) -> std::time::Duration {
    #[cfg(debug_assertions)]
    if let Ok(raw) = std::env::var(var) {
        if let Ok(ms) = raw.parse::<u64>() {
            if accept_zero || ms > 0 {
                return std::time::Duration::from_millis(ms);
            }
        }
    }
    default
}

/// Sibling of [`debug_env_duration_ms`] for a `usize` knob (debug builds only).
///
/// **Release builds always return `default`** — the env read is
/// `#[cfg(debug_assertions)]`, compiled out entirely, so no production environment
/// can alter the value. Debug builds (what `cargo test` builds) honor `var` when it
/// parses to a `usize`, read at every call so a test scopes the override to itself.
/// Used by the per-generation read-pool cap (`crate::env`): tests force a
/// tiny cap to prove wait-at-cap behavior deterministically.
#[cfg_attr(not(debug_assertions), allow(unused_variables))]
pub(crate) fn debug_env_usize(var: &str, default: usize) -> usize {
    #[cfg(debug_assertions)]
    if let Ok(raw) = std::env::var(var) {
        if let Ok(value) = raw.parse::<usize>() {
            return value;
        }
    }
    default
}

/// The write-lock acquire timeout used by the cache write paths. Release builds
/// always return [`DEFAULT_WRITE_LOCK_TIMEOUT`] (5s); debug builds honor
/// `NORN_CACHE_LOCK_TIMEOUT_MS` when it parses to a POSITIVE integer (zero is
/// rejected — see [`debug_env_duration_ms`]).
pub(crate) fn write_lock_timeout() -> std::time::Duration {
    debug_env_duration_ms(
        WRITE_LOCK_TIMEOUT_ENV,
        DEFAULT_WRITE_LOCK_TIMEOUT,
        /* accept_zero = */ false,
    )
}

/// Open (creating if absent) the lock file at `lock_path` and acquire an
/// exclusive advisory flock, polling until `timeout`. Returns the open
/// `File` (caller keeps it alive to hold the lock) or an `Io` error on
/// open failure.
///
/// The caller is responsible for mapping the timeout-`Err` to the correct
/// `CacheError` variant — `WriteLock` maps to `LockTimeout`;
/// `MutationLock` maps to `MutationLockTimeout`.
pub(crate) fn acquire_flock(
    lock_path: &Utf8Path,
    timeout: std::time::Duration,
) -> Result<std::fs::File, std::io::Error> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path.as_std_path())?;

    let deadline = std::time::Instant::now() + timeout;
    let interval = std::time::Duration::from_millis(25);
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            // Genuine contention: the platform's non-blocking lock call maps to
            // `ErrorKind::WouldBlock` (EWOULDBLOCK/EAGAIN under `flock(2)`'s
            // `LOCK_NB`, per `fs2::lock_contended_error`). Only this class waits
            // out the deadline before reporting timeout; at `Duration::ZERO`
            // (the callers that must never block, e.g. `cache clear`) the very
            // first contended attempt already reports timeout.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "lock timeout",
                    ));
                }
                std::thread::sleep(interval);
            }
            // Any other failure is a real fault (e.g. a filesystem/locking
            // error), not contention — propagate it immediately rather than
            // waiting out the deadline and collapsing it into a synthesized
            // `WouldBlock`, which would misreport it as another process
            // holding the lock (every caller distinguishes `WouldBlock` from
            // everything else to decide "contention" vs "real error").
            Err(e) => return Err(e),
        }
    }
}

pub struct WriteLock {
    _file: std::fs::File,
}

impl WriteLock {
    /// Try to acquire an exclusive advisory lock on `<cache_dir>/.lock`,
    /// polling until the deadline. Returns `CacheError::LockTimeout` if
    /// another holder is still holding the lock at deadline.
    pub fn acquire(cache_dir: &Utf8Path, timeout: std::time::Duration) -> Result<Self, CacheError> {
        let lock_path = cache_dir.join(".lock");
        acquire_flock(&lock_path, timeout)
            .map(|f| WriteLock { _file: f })
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    CacheError::LockTimeout
                } else {
                    CacheError::Io {
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

    /// The production default is exactly 5s, and release builds compile the env
    /// read out entirely (`#[cfg(debug_assertions)]`), so the test injection
    /// cannot change production behavior. This pins the default; the env-unset
    /// guard keeps the assert honest when a debug dev shell happens to export
    /// the override. (No in-binary test mutates the process env — the env path
    /// is exercised only by integration tests on their own child processes.)
    #[test]
    fn write_lock_timeout_defaults_to_five_seconds() {
        assert_eq!(
            DEFAULT_WRITE_LOCK_TIMEOUT,
            std::time::Duration::from_secs(5)
        );
        #[cfg(debug_assertions)]
        if std::env::var(WRITE_LOCK_TIMEOUT_ENV).is_ok() {
            return;
        }
        assert_eq!(write_lock_timeout(), DEFAULT_WRITE_LOCK_TIMEOUT);
    }

    #[test]
    fn lock_acquires_when_free() {
        let tmp = TempDir::new().unwrap();
        let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let guard = WriteLock::acquire(&dir, std::time::Duration::from_millis(100)).unwrap();
        drop(guard);
    }

    #[test]
    fn lock_blocks_second_holder() {
        let tmp = TempDir::new().unwrap();
        let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let _guard1 = WriteLock::acquire(&dir, std::time::Duration::from_millis(100)).unwrap();
        let result = WriteLock::acquire(&dir, std::time::Duration::from_millis(100));
        assert!(matches!(result, Err(crate::cache::CacheError::LockTimeout)));
    }

    #[test]
    fn acquire_flock_free_path() {
        let tmp = TempDir::new().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("test.lock")).unwrap();
        let file = acquire_flock(&path, std::time::Duration::from_millis(100)).unwrap();
        drop(file);
    }

    #[test]
    fn acquire_flock_timeout_path() {
        let tmp = TempDir::new().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("test.lock")).unwrap();
        let _held = acquire_flock(&path, std::time::Duration::from_millis(200)).unwrap();
        let result = acquire_flock(&path, std::time::Duration::from_millis(100));
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    }

    /// Pins the property a zero-timeout caller (`cache clear`, NRN-270) relies
    /// on: a lock-file open failure that ISN'T contention (here, the parent
    /// directory never existed) must surface with its own real `ErrorKind`
    /// rather than being collapsed into the synthesized `WouldBlock` "lock
    /// timeout" the contention path returns. This exercises the `open(...)?`
    /// step specifically; see the module doc on why a genuine non-`WouldBlock`
    /// failure from `try_lock_exclusive` itself isn't practical to synthesize
    /// portably in a unit test (it would need OS-level fault injection).
    #[test]
    fn acquire_flock_missing_parent_dir_is_not_found_not_would_block() {
        let tmp = TempDir::new().unwrap();
        let path =
            Utf8PathBuf::from_path_buf(tmp.path().join("never-created").join(".lock")).unwrap();
        let result = acquire_flock(&path, std::time::Duration::ZERO);
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::NotFound,
            "a missing parent dir must propagate as NotFound, not collapse into WouldBlock"
        );
    }
}
