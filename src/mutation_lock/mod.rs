pub mod pending;

use crate::cache::{acquire_flock, CacheError};
use camino::Utf8Path;
use std::time::Duration;

pub const MUTATION_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct MutationLock {
    _file: std::fs::File,
}

impl MutationLock {
    pub fn acquire_if_mutating(
        state_dir: &Utf8Path,
        is_apply: bool,
    ) -> Result<Option<Self>, CacheError> {
        Self::acquire_with_timeout(state_dir, is_apply, MUTATION_LOCK_TIMEOUT)
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
