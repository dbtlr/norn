// src/mutation_lock/pending.rs

use crate::cache::hex_lower;
use crate::cache::CacheError;
use camino::{Utf8Path, Utf8PathBuf};
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime};

/// How long pending plan files are kept before the TTL sweep removes them.
#[allow(dead_code)]
pub(crate) const PENDING_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// Compute the content-addressed filename for a pending plan.
/// Uses SHA-256 of the raw plan bytes so re-blocking the same plan is idempotent.
fn pending_filename(raw_plan: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw_plan.as_bytes());
    format!("{}.plan.json", hex_lower(hasher.finalize().as_ref()))
}

/// Save `raw_plan` to `<state_dir>/pending/<hash>.plan.json`. Returns the
/// path written. If the write fails, returns the IO error (caller decides
/// whether to surface it as a warning).
#[allow(dead_code)]
pub fn save_pending_plan(state_dir: &Utf8Path, raw_plan: &str) -> Result<Utf8PathBuf, CacheError> {
    let pending_dir = state_dir.join("pending");
    std::fs::create_dir_all(pending_dir.as_std_path()).map_err(|e| CacheError::MutationLockIo {
        path: pending_dir.clone(),
        source: e,
    })?;
    let filename = pending_filename(raw_plan);
    let path = pending_dir.join(filename);
    std::fs::write(path.as_std_path(), raw_plan).map_err(|e| CacheError::MutationLockIo {
        path: path.clone(),
        source: e,
    })?;
    Ok(path)
}

/// Delete a previously-saved pending plan file. Best-effort: silently ignores
/// errors (file already gone, permissions, etc.).
#[allow(dead_code)]
pub fn delete_pending_plan(path: &Utf8Path) {
    let _ = std::fs::remove_file(path.as_std_path());
}

/// Remove pending plan files whose mtime is older than `PENDING_TTL_SECS`.
/// Best-effort — a failing sweep never fails the calling mutation.
#[allow(dead_code)]
pub fn sweep_pending(state_dir: &Utf8Path) {
    sweep_pending_older_than(state_dir, PENDING_TTL_SECS);
}

/// Testable variant of `sweep_pending`: removes `.plan.json` files whose mtime
/// is more than `older_than_secs` seconds in the past.
pub(crate) fn sweep_pending_older_than(state_dir: &Utf8Path, older_than_secs: u64) {
    let pending_dir = state_dir.join("pending");
    let cutoff = match SystemTime::now().checked_sub(Duration::from_secs(older_than_secs)) {
        Some(t) => t,
        None => return,
    };
    let rd = match std::fs::read_dir(pending_dir.as_std_path()) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".plan.json") {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            if let Ok(modified) = meta.modified() {
                if modified < cutoff {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn state_dir(tmp: &TempDir) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap()
    }

    #[test]
    fn save_creates_file_at_content_hash_path() {
        let tmp = TempDir::new().unwrap();
        let sd = state_dir(&tmp);
        let path = save_pending_plan(&sd, r#"{"schema_version":1}"#).unwrap();
        assert!(path.as_std_path().exists());
        assert!(path.as_str().ends_with(".plan.json"));
        // Content round-trips.
        let read_back = std::fs::read_to_string(path.as_std_path()).unwrap();
        assert_eq!(read_back, r#"{"schema_version":1}"#);
    }

    #[test]
    fn save_is_idempotent_for_same_content() {
        let tmp = TempDir::new().unwrap();
        let sd = state_dir(&tmp);
        let p1 = save_pending_plan(&sd, "same content").unwrap();
        let p2 = save_pending_plan(&sd, "same content").unwrap();
        assert_eq!(p1, p2, "same content must map to the same path");
        // Only one file exists.
        let count = std::fs::read_dir(tmp.path().join("pending"))
            .unwrap()
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn different_content_produces_different_paths() {
        let tmp = TempDir::new().unwrap();
        let sd = state_dir(&tmp);
        let p1 = save_pending_plan(&sd, "plan one").unwrap();
        let p2 = save_pending_plan(&sd, "plan two").unwrap();
        assert_ne!(p1, p2, "different content must map to different paths");
    }

    #[test]
    fn delete_removes_file() {
        let tmp = TempDir::new().unwrap();
        let sd = state_dir(&tmp);
        let path = save_pending_plan(&sd, "plan content").unwrap();
        assert!(path.as_std_path().exists());
        delete_pending_plan(&path);
        assert!(!path.as_std_path().exists());
    }

    #[test]
    fn delete_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let sd = state_dir(&tmp);
        let path = save_pending_plan(&sd, "plan").unwrap();
        delete_pending_plan(&path);
        // Calling again on missing file must not panic.
        delete_pending_plan(&path);
    }

    #[test]
    fn sweep_with_zero_age_removes_all_plan_files() {
        let tmp = TempDir::new().unwrap();
        let sd = state_dir(&tmp);
        save_pending_plan(&sd, "plan one").unwrap();
        save_pending_plan(&sd, "plan two").unwrap();
        // older_than_secs=0 means cutoff=now → all files were created before now.
        sweep_pending_older_than(&sd, 0);
        let count = std::fs::read_dir(tmp.path().join("pending"))
            .unwrap()
            .count();
        assert_eq!(count, 0);
    }

    #[test]
    fn sweep_with_large_age_keeps_all_files() {
        let tmp = TempDir::new().unwrap();
        let sd = state_dir(&tmp);
        save_pending_plan(&sd, "plan one").unwrap();
        // older_than_secs=u64::MAX → checked_sub returns None → early return (no deletions).
        sweep_pending_older_than(&sd, u64::MAX);
        let count = std::fs::read_dir(tmp.path().join("pending"))
            .unwrap()
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn sweep_ignores_non_plan_files() {
        let tmp = TempDir::new().unwrap();
        let sd = state_dir(&tmp);
        let pending_dir = sd.join("pending");
        std::fs::create_dir_all(pending_dir.as_std_path()).unwrap();
        let unrelated = pending_dir.join("readme.txt");
        std::fs::write(unrelated.as_std_path(), "note").unwrap();
        sweep_pending_older_than(&sd, 0);
        assert!(
            unrelated.as_std_path().exists(),
            "non-.plan.json files must survive sweep"
        );
    }
}
