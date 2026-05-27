//! Advisory file lock for serializing cache write operations.
//!
//! Acquires an exclusive `flock(2)` (via fs2) on `<cache_dir>/.lock`.
//! Reads never block; only `Cache::rebuild` / `Cache::index_incremental`
//! (and other write paths) take this lock. WAL mode (set at open time)
//! is what makes concurrent reads safe alongside an in-flight write.

use camino::Utf8Path;
use fs2::FileExt;
use std::fs::OpenOptions;

use crate::cache::error::CacheError;

pub struct WriteLock {
    _file: std::fs::File,
}

impl WriteLock {
    /// Try to acquire an exclusive advisory lock on `<cache_dir>/.lock`,
    /// polling until the deadline. Returns `CacheError::LockTimeout` if
    /// another holder is still holding the lock at deadline.
    pub fn acquire(cache_dir: &Utf8Path, timeout: std::time::Duration) -> Result<Self, CacheError> {
        let lock_path = cache_dir.join(".lock");
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path.as_std_path())
            .map_err(|e| CacheError::Io {
                path: lock_path.clone(),
                source: e,
            })?;

        let deadline = std::time::Instant::now() + timeout;
        let interval = std::time::Duration::from_millis(25);
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(WriteLock { _file: file }),
                Err(_) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(CacheError::LockTimeout);
                    }
                    std::thread::sleep(interval);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

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
}
