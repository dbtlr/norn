//! Request-boundary freshness probe — the trust seam (ADR 0013 Phase 2, ADR
//! 0017 owner-held trust).
//!
//! One authority interface answers "may this generation serve this request".
//! A warm read runs directly iff its probe passes; a Stale verdict routes it
//! through the coalesced liveness refresh first. Splitting the cheap read-side
//! PROBE from the write-side REFRESH is what lets concurrent reads of an
//! unchanged vault skip the writer queue entirely.
//!
//! [`FreshnessProbe`] is the named interface. The phase-2 implementation is the
//! read-only [`StatSweepProbe`] (stat-sweep semantics at the request
//! boundary). A phase-4 watcher-events probe
//! slots in behind the same trait as a new implementation without touching this
//! signature; the increment pipeline consumes this authority rather than
//! embedding its own filesystem re-proof. The probe/change-detection interface
//! is file-kind-agnostic by design; the phase-2 walk covers `.md` only.

use std::ops::ControlFlow;

use camino::{Utf8Path, Utf8PathBuf};

use crate::cache::change_detection::{load_cached_metadata, walk_markdown_files};
use crate::cache::error::CacheError;
use crate::cache::Cache;

/// The verdict of a [`FreshnessProbe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Freshness {
    /// The cache reflects the live filesystem; the held read may be served
    /// directly, with no refresh.
    Fresh,
    /// The cache is behind the filesystem; the request must run a refresh before
    /// serving. The reason names the first divergence found.
    Stale(StaleReason),
}

/// Why a probe judged the cache stale. The first divergence found — kept because
/// it is cheap: the walk short-circuits at the first added/modified file, and a
/// deletion falls out of a whole-walk file-count shortfall against the baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StaleReason {
    /// The cache has never been fully built (no `last_full_rebuild_ts` meta row).
    NeverBuilt,
    /// A tracked markdown file's `(mtime_ns, size_bytes)` diverged.
    Modified(Utf8PathBuf),
    /// A markdown file exists on disk with no baseline row (created / un-ignored).
    Added(Utf8PathBuf),
    /// The clean walk saw fewer tracked files than the baseline: at least one
    /// tracked file was deleted (or newly ignored).
    Deleted,
}

/// The named request-boundary freshness interface (the trust seam). A read is
/// served directly iff [`probe`](FreshnessProbe::probe) returns
/// [`Freshness::Fresh`]; a [`Freshness::Stale`] verdict routes the request
/// through the coalesced liveness refresh first.
pub(crate) trait FreshnessProbe {
    /// Judge whether `cache` still reflects the live vault at `vault_root`. A
    /// probe MUST be read-only over the cache.
    fn probe(&self, vault_root: &Utf8Path, cache: &Cache) -> Result<Freshness, CacheError>;
}

/// The read-only stat-sweep freshness probe (see [`FreshnessProbe`]).
///
/// Loads the cached `(path -> mtime,size)` baseline in one query off the
/// caller's read connection, then walks the live filesystem (`change_detection`'s
/// shared walk, identical ignore rules) and returns Stale at the FIRST
/// added/modified file. Deletions are caught after a clean walk by a baseline
/// file-count shortfall. A Fresh verdict here implies `detect` would report zero
/// changes.
pub(crate) struct StatSweepProbe;

impl FreshnessProbe for StatSweepProbe {
    fn probe(&self, vault_root: &Utf8Path, cache: &Cache) -> Result<Freshness, CacheError> {
        if !cache.has_been_built()? {
            return Ok(Freshness::Stale(StaleReason::NeverBuilt));
        }

        let baseline = load_cached_metadata(&cache.conn)?;

        let mut seen = 0usize;
        let outcome = walk_markdown_files(
            vault_root,
            &cache.files_ignore,
            &mut |rel: &Utf8Path, mtime_ns: i64, size_bytes: i64| match baseline.get(rel) {
                Some(meta) => {
                    if mtime_ns == meta.mtime_ns && size_bytes == meta.size_bytes {
                        seen += 1;
                        ControlFlow::Continue(())
                    } else {
                        ControlFlow::Break(StaleReason::Modified(rel.to_owned()))
                    }
                }
                None => ControlFlow::Break(StaleReason::Added(rel.to_owned())),
            },
        )?;

        if let ControlFlow::Break(reason) = outcome {
            return Ok(Freshness::Stale(reason));
        }

        if seen < baseline.len() {
            return Ok(Freshness::Stale(StaleReason::Deleted));
        }

        Ok(Freshness::Fresh)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    fn setup() -> (TempDir, Utf8PathBuf, Cache) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(root.join("a.md").as_std_path(), "---\ntitle: A\n---\n").unwrap();
        std::fs::write(root.join("b.md").as_std_path(), "---\ntitle: B\n---\n").unwrap();
        let mut cache = Cache::open(&root).unwrap();
        cache.full_build(&root).unwrap();
        (tmp, root, cache)
    }

    #[test]
    fn unbuilt_cache_is_stale_never_built() {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(root.join("a.md").as_std_path(), "---\ntitle: A\n---\n").unwrap();
        let cache = Cache::open(&root).unwrap();
        assert_eq!(
            StatSweepProbe.probe(&root, &cache).unwrap(),
            Freshness::Stale(StaleReason::NeverBuilt),
        );
    }

    #[test]
    fn unchanged_vault_is_fresh() {
        let (_tmp, root, cache) = setup();
        assert_eq!(
            StatSweepProbe.probe(&root, &cache).unwrap(),
            Freshness::Fresh
        );
    }

    #[test]
    fn added_file_is_stale() {
        let (_tmp, root, cache) = setup();
        std::fs::write(root.join("c.md").as_std_path(), "---\ntitle: C\n---\n").unwrap();
        assert_eq!(
            StatSweepProbe.probe(&root, &cache).unwrap(),
            Freshness::Stale(StaleReason::Added("c.md".into())),
        );
    }

    #[test]
    fn modified_file_is_stale() {
        let (_tmp, root, cache) = setup();
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(
            root.join("a.md").as_std_path(),
            "---\ntitle: A2\n---\nedited\n",
        )
        .unwrap();
        assert_eq!(
            StatSweepProbe.probe(&root, &cache).unwrap(),
            Freshness::Stale(StaleReason::Modified("a.md".into())),
        );
    }

    #[test]
    fn deleted_file_is_stale() {
        let (_tmp, root, cache) = setup();
        std::fs::remove_file(root.join("a.md").as_std_path()).unwrap();
        assert_eq!(
            StatSweepProbe.probe(&root, &cache).unwrap(),
            Freshness::Stale(StaleReason::Deleted),
        );
    }

    #[test]
    fn fresh_implies_detect_reports_no_changes() {
        let (_tmp, root, cache) = setup();
        assert_eq!(
            StatSweepProbe.probe(&root, &cache).unwrap(),
            Freshness::Fresh
        );
        let outcome = crate::cache::change_detection::detect(
            &root,
            &cache,
            &crate::cache::ChangeDetectOptions::default(),
        )
        .unwrap();
        assert!(
            outcome.changes.is_empty(),
            "fresh probe but detect saw {:?}",
            outcome.changes
        );
    }
}
