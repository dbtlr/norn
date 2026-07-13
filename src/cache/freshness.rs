//! Request-boundary freshness probe (ADR 0013 Phase 2, NRN-253).
//!
//! A warm read runs concurrently iff its freshness probe passes; a Stale verdict
//! routes it through the coalesced liveness refresh first, then serves. Splitting
//! the cheap read-side PROBE from the write-side REFRESH is what lets concurrent
//! reads of an unchanged vault skip the writer queue entirely (they previously
//! each submitted a refresh op) — see `crate::mcp::context`'s per-request
//! pipeline for how the two halves compose.
//!
//! [`FreshnessProbe`] is the named interface. Today's only implementation is the
//! read-only [`StatSweepProbe`]; a Phase 3 watcher-events probe (NRN-96) slots in
//! behind the same trait as impl #2 (O(1), steady-state-true). The stat sweep is
//! the PERMANENT demoted-mode probe — Phase 3 keeps it as the fallback whenever
//! the watcher is unavailable or still warming up, so it is not throwaway
//! scaffolding.

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
///
/// Production discards the reason today (a Stale verdict routes to the refresh
/// regardless of why); its consumers are the probe's tests. It stays on the
/// verdict because it costs nothing to carry, and a diagnostics consumer — e.g.
/// Phase 3 shadow-verify or stale-probe logging — is plausible future work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StaleReason {
    /// The cache has never been fully built (no `last_full_rebuild_ts` meta row).
    /// A markdown-only walk can't validate it — a full rebuild also populates
    /// attachments and other non-markdown files the walk never sees — so the
    /// probe reports Stale to run the rebuild, mirroring `index_incremental`'s
    /// own defer-to-`rebuild` on an unbuilt cache.
    NeverBuilt,
    /// A tracked markdown file's `(mtime_ns, size_bytes)` diverged from the
    /// baseline.
    Modified(Utf8PathBuf),
    /// A markdown file exists on disk with no baseline row (created / newly
    /// un-ignored).
    Added(Utf8PathBuf),
    /// The clean walk saw fewer tracked files than the baseline: at least one
    /// tracked file was deleted (or newly ignored). No single path is named —
    /// pinning it needs a full set-difference, not worth the cost on the hot
    /// read path; the refresh's `detect` names it precisely when it runs.
    Deleted,
}

/// The named request-boundary freshness interface (ADR 0013 Phase 2). A read is
/// served concurrently iff [`probe`](FreshnessProbe::probe) returns
/// [`Freshness::Fresh`]; a [`Freshness::Stale`] verdict routes the request through
/// the coalesced liveness refresh first.
///
/// Two implementations are foreseen behind this one trait:
///
/// - [`StatSweepProbe`] (this commit) — an O(vault) read-only stat sweep, and the
///   PERMANENT demoted-mode probe (Phase 3 keeps it as the watcher's fallback).
/// - a watcher-events probe (Phase 3, NRN-96) — O(1), steady-state-true — slotting
///   in as impl #2 without touching this signature.
pub(crate) trait FreshnessProbe {
    /// Judge whether `cache` still reflects the live vault at `vault_root`. A
    /// probe MUST be read-only over the cache: it may only read, never write,
    /// through the connection it is handed.
    fn probe(&self, vault_root: &Utf8Path, cache: &Cache) -> Result<Freshness, CacheError>;
}

/// The read-only stat-sweep freshness probe (see [`FreshnessProbe`]).
///
/// Loads the cached `(path -> mtime,size)` baseline in one query off the caller's
/// read connection, then walks the live filesystem (`change_detection`'s shared
/// walk, identical ignore rules) and returns Stale at the FIRST added/modified
/// file. Deletions are caught after a clean walk by a baseline file-count
/// shortfall. It deliberately does NOT hash-verify a stat divergence the way
/// `detect` does: a false Stale only costs one no-op refresh, never a stale read,
/// and the cheap-check equality it uses is exactly `detect`'s — so a Fresh
/// verdict here implies `detect` would report zero changes.
pub(crate) struct StatSweepProbe;

impl FreshnessProbe for StatSweepProbe {
    fn probe(&self, vault_root: &Utf8Path, cache: &Cache) -> Result<Freshness, CacheError> {
        // An unbuilt cache cannot be validated by a markdown-only walk (it must
        // rebuild to populate attachments); report Stale so the refresh rebuilds.
        if !cache.has_been_built()? {
            return Ok(Freshness::Stale(StaleReason::NeverBuilt));
        }

        // Baseline in ONE query off the read connection — `change_detection`'s
        // own loader, so the probe reads exactly the rows `detect` would compare
        // against (it simply never looks at the hash: a stat divergence goes
        // straight to Stale rather than hash-verifying). Sharing the loader means
        // a future `documents`-table change cannot silently diverge the probe's
        // Fresh verdict from `detect`'s.
        let baseline = load_cached_metadata(&cache.conn)?;

        // Walk with the SAME ignore rules `detect` uses, short-circuiting at the
        // first divergence. `seen` counts baseline hits so a clean walk shorter
        // than the baseline reveals a deletion.
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

        // The walk completed with no add/modify, so every live file matched a
        // distinct baseline entry. A shortfall therefore means a baseline file is
        // gone from disk (deleted or newly ignored).
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

    /// A built cache over a 2-doc vault (mirrors `change_detection`'s fixture).
    fn setup() -> (TempDir, Utf8PathBuf, Cache) {
        let tmp = TempDir::new().unwrap();
        // TempDir's own basename starts with `.tmp`, which the walk treats as
        // hidden and skips; nest the vault under a non-hidden subdir.
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(root.join("a.md").as_std_path(), "---\ntitle: A\n---\n").unwrap();
        std::fs::write(root.join("b.md").as_std_path(), "---\ntitle: B\n---\n").unwrap();
        let mut cache = Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();
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
        // Opened but never rebuilt: no `last_full_rebuild_ts`.
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

    /// The easy miss: a deletion-only change (no add, no modify) must still read
    /// Stale — caught by the whole-walk file-count shortfall against the baseline.
    #[test]
    fn deleted_file_is_stale() {
        let (_tmp, root, cache) = setup();
        std::fs::remove_file(root.join("a.md").as_std_path()).unwrap();
        assert_eq!(
            StatSweepProbe.probe(&root, &cache).unwrap(),
            Freshness::Stale(StaleReason::Deleted),
        );
    }

    /// A Fresh verdict must agree with `detect`: if the probe passes, the refresh
    /// it skips would have found zero changes.
    #[test]
    fn fresh_implies_detect_reports_no_changes() {
        let (_tmp, root, cache) = setup();
        assert_eq!(
            StatSweepProbe.probe(&root, &cache).unwrap(),
            Freshness::Fresh
        );
        let changes = crate::cache::change_detection::detect(
            &root,
            &cache,
            &crate::cache::ChangeDetectOptions::default(),
        )
        .unwrap();
        assert!(changes.is_empty(), "fresh probe but detect saw {changes:?}");
    }
}
