//! Detect changes between the cached state and the live filesystem.

use camino::{Utf8Path, Utf8PathBuf};
use std::collections::HashMap;
use std::ops::ControlFlow;

use crate::cache::error::CacheError;

#[derive(Debug, Clone, Default)]
pub struct ChangeDetectOptions {
    /// Skip mtime+size cheap-check; hash every file. Use on filesystems
    /// where mtime is unreliable (NFS, Docker bind-mounts, etc.).
    pub force_hash: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChange {
    Added(Utf8PathBuf),
    Modified(Utf8PathBuf),
    Deleted(Utf8PathBuf),
}

impl FileChange {
    fn path(&self) -> &Utf8Path {
        match self {
            FileChange::Added(p) | FileChange::Modified(p) | FileChange::Deleted(p) => p,
        }
    }
}

pub fn detect(
    vault_root: &Utf8Path,
    cache: &crate::cache::Cache,
    options: &ChangeDetectOptions,
) -> Result<Vec<FileChange>, CacheError> {
    let cached = load_cached_metadata(&cache.conn)?;
    // Honor files.ignore in the live scan so a path newly added to files.ignore
    // is seen as absent → detected as Deleted → purged from the cache, keeping
    // the incremental path in agreement with a full rebuild (NRN-117).
    let live = scan_filesystem(vault_root, &cache.files_ignore)?;

    let mut changes = Vec::new();

    for (path, live_meta) in &live {
        match cached.get(path) {
            Some(cached_meta) => {
                let unchanged_cheap = !options.force_hash
                    && live_meta.mtime_ns == cached_meta.mtime_ns
                    && live_meta.size_bytes == cached_meta.size_bytes;
                if unchanged_cheap {
                    continue;
                }
                // Cheap-check failed or force_hash. Verify by hash.
                let live_hash = hash_file(&vault_root.join(path))?;
                if live_hash != cached_meta.hash {
                    changes.push(FileChange::Modified(path.clone()));
                }
                // If hash matches, the file is unchanged content-wise; mtime
                // drift only. The writer will refresh mtime opportunistically.
            }
            None => {
                changes.push(FileChange::Added(path.clone()));
            }
        }
    }

    for path in cached.keys() {
        if !live.contains_key(path) {
            changes.push(FileChange::Deleted(path.clone()));
        }
    }

    changes.sort_by(|a, b| a.path().cmp(b.path()));
    Ok(changes)
}

struct FileMeta {
    mtime_ns: i64,
    size_bytes: i64,
    hash: String,
}

fn load_cached_metadata(
    conn: &rusqlite::Connection,
) -> Result<HashMap<Utf8PathBuf, FileMeta>, CacheError> {
    let mut stmt = conn.prepare("SELECT path, mtime_ns, size_bytes, hash FROM documents")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut out = HashMap::new();
    for r in rows {
        let (path, mtime_ns, size_bytes, hash) = r?;
        out.insert(
            Utf8PathBuf::from(path),
            FileMeta {
                mtime_ns,
                size_bytes,
                hash,
            },
        );
    }
    Ok(out)
}

struct LiveMeta {
    mtime_ns: i64,
    size_bytes: i64,
}

fn scan_filesystem(
    root: &Utf8Path,
    ignore: &[String],
) -> Result<HashMap<Utf8PathBuf, LiveMeta>, CacheError> {
    let mut out = HashMap::new();
    let _ = walk_markdown_files(root, ignore, &mut |rel: &Utf8Path, mtime_ns, size_bytes| {
        out.insert(
            rel.to_owned(),
            LiveMeta {
                mtime_ns,
                size_bytes,
            },
        );
        ControlFlow::<()>::Continue(())
    })?;
    Ok(out)
}

/// Walk `root`'s markdown files — applying the SAME hidden-path skip and
/// `files.ignore` rules `detect`'s full scan uses — invoking `visit` with each
/// file's vault-relative path and cheap-check stat `(mtime_ns, size_bytes)`.
/// `visit` returns [`ControlFlow::Break`] to stop the walk early and propagate a
/// value out; [`ControlFlow::Continue`] keeps walking.
///
/// Factored out so `detect`'s full scan and the NRN-253 request-boundary
/// freshness probe (`crate::cache::freshness`) share ONE walk. The probe MUST see
/// the same files, with the same ignore semantics and the same `(mtime_ns,
/// size_bytes)` extraction, that a subsequent `detect`/refresh would — otherwise
/// a Fresh verdict and the refresh could disagree about what "changed" means.
/// `scan_filesystem` always continues (collecting the full map); the probe breaks
/// at the first divergence.
pub(crate) fn walk_markdown_files<B>(
    root: &Utf8Path,
    ignore: &[String],
    visit: &mut impl FnMut(&Utf8Path, i64, i64) -> ControlFlow<B>,
) -> Result<ControlFlow<B>, CacheError> {
    walk_visit(root, root, ignore, visit)
}

fn walk_visit<B>(
    base: &Utf8Path,
    dir: &Utf8Path,
    ignore: &[String],
    visit: &mut impl FnMut(&Utf8Path, i64, i64) -> ControlFlow<B>,
) -> Result<ControlFlow<B>, CacheError> {
    for entry in std::fs::read_dir(dir.as_std_path()).map_err(|e| CacheError::Io {
        path: dir.to_owned(),
        source: e,
    })? {
        let entry = entry.map_err(|e| CacheError::Io {
            path: dir.to_owned(),
            source: e,
        })?;
        let path_buf = entry.path();
        let path = match Utf8PathBuf::from_path_buf(path_buf) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if path.file_name().is_some_and(|n| n.starts_with('.')) {
            continue;
        }
        let ft = entry.file_type().map_err(|e| CacheError::Io {
            path: path.clone(),
            source: e,
        })?;
        if ft.is_dir() {
            if let ControlFlow::Break(b) = walk_visit(base, &path, ignore, visit)? {
                return Ok(ControlFlow::Break(b));
            }
        } else if ft.is_file() && path.extension() == Some("md") {
            let rel = path.strip_prefix(base).unwrap_or(&path).to_owned();
            if crate::graph::is_ignored(&rel, ignore) {
                continue;
            }
            let meta = entry.metadata().map_err(|e| CacheError::Io {
                path: path.clone(),
                source: e,
            })?;
            let mtime_ns = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);
            let size_bytes = meta.len() as i64;
            if let ControlFlow::Break(b) = visit(&rel, mtime_ns, size_bytes) {
                return Ok(ControlFlow::Break(b));
            }
        }
    }
    Ok(ControlFlow::Continue(()))
}

fn hash_file(path: &Utf8Path) -> Result<String, CacheError> {
    // Must match the hash format crate::graph::build_index stores in
    // documents.hash (blake3 hex of the raw file bytes). Mismatching the
    // algorithm would make every force_hash run flag every file as Modified.
    let bytes = std::fs::read(path.as_std_path()).map_err(|e| CacheError::IndexRead {
        path: path.to_owned(),
        source: e,
    })?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    fn setup() -> (TempDir, Utf8PathBuf, crate::cache::Cache) {
        let tmp = TempDir::new().unwrap();
        // Create the vault under a non-hidden subdirectory: TempDir's own
        // basename starts with `.tmp`, which vault_graph's WalkDir filter
        // treats as hidden and skips entirely.
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(root.join("a.md").as_std_path(), "---\ntitle: A\n---\n").unwrap();
        std::fs::write(root.join("b.md").as_std_path(), "---\ntitle: B\n---\n").unwrap();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();
        (tmp, root, cache)
    }

    #[test]
    fn unchanged_files_yield_no_changes() {
        let (_tmp, root, cache) = setup();
        let opts = ChangeDetectOptions::default();
        let changes = detect(&root, &cache, &opts).unwrap();
        assert!(changes.is_empty(), "expected no changes, got {:?}", changes);
    }

    #[test]
    fn modified_file_detected_via_mtime() {
        let (_tmp, root, cache) = setup();
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(
            root.join("a.md").as_std_path(),
            "---\ntitle: A2\n---\nedited\n",
        )
        .unwrap();
        let opts = ChangeDetectOptions::default();
        let changes = detect(&root, &cache, &opts).unwrap();
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0], FileChange::Modified(_)));
    }

    #[test]
    fn added_file_detected() {
        let (_tmp, root, cache) = setup();
        std::fs::write(root.join("c.md").as_std_path(), "---\ntitle: C\n---\n").unwrap();
        let opts = ChangeDetectOptions::default();
        let changes = detect(&root, &cache, &opts).unwrap();
        assert_eq!(changes.len(), 1);
        match &changes[0] {
            FileChange::Added(p) => assert_eq!(p, "c.md"),
            other => panic!("expected Added, got {other:?}"),
        }
    }

    #[test]
    fn deleted_file_detected() {
        let (_tmp, root, cache) = setup();
        std::fs::remove_file(root.join("a.md").as_std_path()).unwrap();
        let opts = ChangeDetectOptions::default();
        let changes = detect(&root, &cache, &opts).unwrap();
        assert_eq!(changes.len(), 1);
        match &changes[0] {
            FileChange::Deleted(p) => assert_eq!(p, "a.md"),
            other => panic!("expected Deleted, got {other:?}"),
        }
    }

    #[test]
    fn force_hash_skips_cheap_check() {
        let (_tmp, root, cache) = setup();
        // No file change. With force_hash, every file gets read but hashes match.
        let opts = ChangeDetectOptions { force_hash: true };
        let changes = detect(&root, &cache, &opts).unwrap();
        assert!(changes.is_empty());
    }
}
