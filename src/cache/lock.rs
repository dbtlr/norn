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

/// Test-only override for the write-lock acquire timeout, in milliseconds
/// (mirrors the `NORN_SERVICE_HANDSHAKE_TIMEOUT_MS` idiom in `service`). Lets
/// contention tests hit `CacheError::LockTimeout` in ~100–200ms instead of
/// stalling the suite the full 5s per contended acquire. NOT an operator knob:
/// it is undocumented outside this file, and production deployments never set
/// it — every production path resolves to [`DEFAULT_WRITE_LOCK_TIMEOUT`]
/// (asserted by `write_lock_timeout_defaults_to_five_seconds` below).
const WRITE_LOCK_TIMEOUT_ENV: &str = "NORN_CACHE_LOCK_TIMEOUT_MS";

/// The write-lock acquire timeout: [`DEFAULT_WRITE_LOCK_TIMEOUT`] unless the
/// test-only [`WRITE_LOCK_TIMEOUT_ENV`] parses to a positive integer. Read at
/// every acquire (not cached) so a test can scope the override to one section.
pub(crate) fn write_lock_timeout() -> std::time::Duration {
    match std::env::var(WRITE_LOCK_TIMEOUT_ENV) {
        Ok(raw) => match raw.parse::<u64>() {
            Ok(ms) if ms > 0 => std::time::Duration::from_millis(ms),
            _ => DEFAULT_WRITE_LOCK_TIMEOUT,
        },
        Err(_) => DEFAULT_WRITE_LOCK_TIMEOUT,
    }
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
            Err(_) => {
                if std::time::Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "lock timeout",
                    ));
                }
                std::thread::sleep(interval);
            }
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

    /// The production default is exactly 5s. The env override is test-only; this
    /// pins the default so the injection cannot silently change production
    /// behavior. (Env READS are safe under parallel tests; the contention tests
    /// that SET the var affect only contended acquires, which no other test
    /// stages concurrently.)
    #[test]
    fn write_lock_timeout_defaults_to_five_seconds() {
        assert_eq!(
            DEFAULT_WRITE_LOCK_TIMEOUT,
            std::time::Duration::from_secs(5)
        );
        if std::env::var(WRITE_LOCK_TIMEOUT_ENV).is_err() {
            assert_eq!(write_lock_timeout(), DEFAULT_WRITE_LOCK_TIMEOUT);
        }
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
}
