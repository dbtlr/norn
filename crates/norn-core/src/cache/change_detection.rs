//! Detect changes between the cached state and the live filesystem.
//!
//! The change-detection interface is file-kind-agnostic by design; the phase-2
//! implementation walks `.md` files only. A future watcher
//! authority slots in behind the same shape.

use camino::{Utf8Path, Utf8PathBuf};
use std::collections::HashMap;
use std::ops::ControlFlow;

use crate::cache::error::CacheError;

#[derive(Debug, Clone, Default)]
pub struct ChangeDetectOptions {
    /// Skip mtime+size cheap-check; hash every file. Use on filesystems where
    /// mtime is unreliable (NFS, Docker bind-mounts, etc.).
    pub force_hash: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChange {
    Added(Utf8PathBuf),
    Modified(Utf8PathBuf),
    Deleted(Utf8PathBuf),
}

impl FileChange {
    pub(crate) fn path(&self) -> &Utf8Path {
        match self {
            FileChange::Added(p) | FileChange::Modified(p) | FileChange::Deleted(p) => p,
        }
    }
}

/// The outcome of a change scan: the content changes to re-index, plus the set
/// of touched-but-unchanged files whose cached `(mtime,size)` baseline must be
/// re-stamped so the freshness probe converges.
pub(crate) struct DetectOutcome {
    pub(crate) changes: Vec<FileChange>,
    /// `(path, (mtime_ns, size_bytes))` for files whose cheap-check failed but
    /// whose content hash still matches the cached hash — the metadata drifted
    /// (e.g. a same-bytes rewrite bumped mtime) without any content change. Each
    /// metadata pair comes from the SAME stable observation that produced the
    /// matching hash (never a fresh naked stat), so a rebaseline can never encode
    /// content that changed during the read.
    pub(crate) rebaselines: Vec<(Utf8PathBuf, (i64, i64))>,
}

pub(crate) fn detect(
    vault_root: &Utf8Path,
    cache: &crate::cache::Cache,
    options: &ChangeDetectOptions,
) -> Result<DetectOutcome, CacheError> {
    let cached = load_cached_metadata(&cache.conn)?;
    // Honor files.ignore in the live scan so a path newly added to files.ignore
    // is seen as absent → detected as Deleted → purged from the cache, keeping
    // the incremental path in agreement with a full build (NRN-117).
    let live = scan_filesystem(vault_root, &cache.files_ignore)?;

    let mut changes = Vec::new();
    let mut rebaselines = Vec::new();

    for (path, live_meta) in &live {
        match cached.get(path) {
            Some(cached_meta) => {
                let unchanged_cheap = !options.force_hash
                    && live_meta.mtime_ns == cached_meta.mtime_ns
                    && live_meta.size_bytes == cached_meta.size_bytes;
                if unchanged_cheap {
                    continue;
                }
                // Cheap check failed (or force_hash): re-hash under a stable
                // observation (stat → read → stat) so the verdict — and any
                // rebaseline metadata — is bound to the bytes we hashed.
                match stable_hash_observation(&vault_root.join(path)) {
                    Some((live_hash, observed)) => {
                        if live_hash != cached_meta.hash {
                            changes.push(FileChange::Modified(path.clone()));
                        } else if observed != (cached_meta.mtime_ns, cached_meta.size_bytes) {
                            // Content identical, only `(mtime,size)` drifted:
                            // rebaseline so the probe stops re-firing (and the
                            // refresh stops re-hashing this file) every request.
                            rebaselines.push((path.clone(), observed));
                        }
                        // else: force_hash on a genuinely unchanged file — no-op.
                    }
                    None => {
                        // The file changed under our own read (before != after):
                        // skip both the change and the rebaseline. The next probe
                        // re-fires once it settles — the safe direction, never a
                        // stale-serve.
                    }
                }
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
    rebaselines.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(DetectOutcome {
        changes,
        rebaselines,
    })
}

/// Per-file metadata cached in the `documents` table. Shared (with
/// [`load_cached_metadata`]) so the freshness probe consumes the SAME loader
/// `detect` does — the probe ignores `hash` (a stat divergence goes straight to
/// Stale), but sharing the query keeps a future `documents`-table change from
/// silently diverging the probe's Fresh verdict from `detect`'s.
pub(crate) struct FileMeta {
    pub(crate) mtime_ns: i64,
    pub(crate) size_bytes: i64,
    hash: String,
}

/// Load the cheap-check baseline — `path -> FileMeta` — from the `documents`
/// table. The one loader shared by `detect` and the freshness probe.
pub(crate) fn load_cached_metadata(
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
/// Factored out so `detect`'s full scan and the request-boundary freshness probe
/// (`crate::cache::freshness`) share ONE walk.
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
        } else if ft.is_file() && crate::graph::is_markdown(&path) {
            let rel = path.strip_prefix(base).unwrap_or(&path);
            if crate::graph::is_ignored(rel, ignore) {
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
            if let ControlFlow::Break(b) = visit(rel, mtime_ns, size_bytes) {
                return Ok(ControlFlow::Break(b));
            }
        }
    }
    Ok(ControlFlow::Continue(()))
}

/// A blake3 hex hash paired with the `(mtime_ns, size_bytes)` observed in the
/// same stable read.
type HashObservation = (String, (i64, i64));

/// Hash `absolute` under a stability guard: stat before, read, stat after. The
/// hash (blake3 hex of the raw bytes, matching `crate::graph::build_index`'s
/// `documents.hash`) is returned WITH the `(mtime_ns, size_bytes)` from that same
/// observation only when the before/after stats agree.
///
/// This is a SPECULATIVE observation whose only consumer is the rebaseline
/// convergence, so it NEVER propagates an error: a missing before-stat, a failed
/// read (the file was deleted between the walk and here, or vanished mid-read), a
/// before/after mismatch, or a size that disagrees with the read all return
/// `None`. Aborting the whole `detect` scan because one file was concurrently
/// deleted would be wrong — that deletion surfaces as `Deleted` on the next
/// cycle. `None` is always the safe direction: skip, and let the next
/// probe/detect classify the path.
fn stable_hash_observation(absolute: &Utf8Path) -> Option<HashObservation> {
    let before = regular_stat(absolute)?;
    let bytes = std::fs::read(absolute.as_std_path()).ok()?;
    let after = regular_stat(absolute)?;
    (before == after && bytes.len() as i64 == after.1)
        .then(|| (blake3::hash(&bytes).to_hex().to_string(), after))
}

fn regular_stat(absolute: &Utf8Path) -> Option<(i64, i64)> {
    let metadata = std::fs::metadata(absolute.as_std_path()).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let mtime_ns = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos() as i64;
    Some((mtime_ns, metadata.len() as i64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    fn setup() -> (TempDir, Utf8PathBuf, crate::cache::Cache) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(root.join("a.md").as_std_path(), "---\ntitle: A\n---\n").unwrap();
        std::fs::write(root.join("b.md").as_std_path(), "---\ntitle: B\n---\n").unwrap();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.full_build(&root).unwrap();
        (tmp, root, cache)
    }

    #[test]
    fn unchanged_files_yield_no_changes() {
        let (_tmp, root, cache) = setup();
        let outcome = detect(&root, &cache, &ChangeDetectOptions::default()).unwrap();
        assert!(
            outcome.changes.is_empty(),
            "expected no changes, got {:?}",
            outcome.changes
        );
        assert!(outcome.rebaselines.is_empty());
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
        let outcome = detect(&root, &cache, &ChangeDetectOptions::default()).unwrap();
        assert_eq!(outcome.changes.len(), 1);
        assert!(matches!(outcome.changes[0], FileChange::Modified(_)));
    }

    #[test]
    fn added_file_detected() {
        let (_tmp, root, cache) = setup();
        std::fs::write(root.join("c.md").as_std_path(), "---\ntitle: C\n---\n").unwrap();
        let outcome = detect(&root, &cache, &ChangeDetectOptions::default()).unwrap();
        assert_eq!(outcome.changes.len(), 1);
        match &outcome.changes[0] {
            FileChange::Added(p) => assert_eq!(p, "c.md"),
            other => panic!("expected Added, got {other:?}"),
        }
    }

    #[test]
    fn deleted_file_detected() {
        let (_tmp, root, cache) = setup();
        std::fs::remove_file(root.join("a.md").as_std_path()).unwrap();
        let outcome = detect(&root, &cache, &ChangeDetectOptions::default()).unwrap();
        assert_eq!(outcome.changes.len(), 1);
        match &outcome.changes[0] {
            FileChange::Deleted(p) => assert_eq!(p, "a.md"),
            other => panic!("expected Deleted, got {other:?}"),
        }
    }

    #[test]
    fn force_hash_skips_cheap_check() {
        let (_tmp, root, cache) = setup();
        let opts = ChangeDetectOptions { force_hash: true };
        let outcome = detect(&root, &cache, &opts).unwrap();
        assert!(outcome.changes.is_empty());
        // force_hash on genuinely-unchanged files rebaselines nothing (the
        // observed metadata already equals the cached baseline).
        assert!(outcome.rebaselines.is_empty());
    }

    /// A same-bytes rewrite bumps mtime without changing content: the cheap check
    /// fails, the re-hash matches, and the path is offered as a rebaseline (not a
    /// content change) so the caller can converge the baseline.
    #[test]
    fn touched_but_unchanged_file_is_offered_as_a_rebaseline() {
        let (_tmp, root, cache) = setup();
        std::thread::sleep(std::time::Duration::from_millis(50));
        // Identical bytes to `setup`'s a.md — only mtime advances.
        std::fs::write(root.join("a.md").as_std_path(), "---\ntitle: A\n---\n").unwrap();

        let outcome = detect(&root, &cache, &ChangeDetectOptions::default()).unwrap();
        assert!(
            outcome.changes.is_empty(),
            "content unchanged → no FileChange, got {:?}",
            outcome.changes
        );
        assert_eq!(outcome.rebaselines.len(), 1);
        assert_eq!(outcome.rebaselines[0].0, "a.md");
    }

    /// A file that vanishes during the speculative rebaseline observation must NOT
    /// abort the whole scan: the observation returns `None` (skip), and the
    /// deletion is classified `Deleted` on the following detect cycle rather than
    /// propagating a `NotFound` error out of `detect`.
    #[test]
    fn vanished_file_during_observation_is_none_and_next_detect_reports_deleted() {
        let (_tmp, root, cache) = setup();

        // The speculative observation of an absent path yields None, not an error.
        assert!(
            super::stable_hash_observation(&root.join("gone.md")).is_none(),
            "a missing file must skip (None), never propagate an IO error"
        );

        // A genuinely-removed tracked file: detect completes without error and
        // classifies it Deleted (the safe classification the skip defers to). The
        // read never fires for it — it drops out of the live walk — but this pins
        // the whole-scan-completes-without-error contract the skip protects.
        std::fs::remove_file(root.join("a.md").as_std_path()).unwrap();
        let outcome = detect(&root, &cache, &ChangeDetectOptions::default()).unwrap();
        assert!(
            outcome
                .changes
                .iter()
                .any(|c| matches!(c, FileChange::Deleted(p) if p == "a.md")),
            "the removed file must be classified Deleted: {:?}",
            outcome.changes
        );
    }
}
