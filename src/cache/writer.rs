//! Cache writer: full rebuild, incremental update, and explicit increment commit.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::core::{Document, GraphIndex, Link, VaultFile};
use camino::{Utf8Path, Utf8PathBuf};
use rusqlite::{params, Transaction, TransactionBehavior};

use crate::cache::change_detection::{detect, ChangeDetectOptions, FileChange};
use crate::cache::error::CacheError;

#[cfg(test)]
std::thread_local! {
    /// Deterministic filesystem-race seam between detection and full parsing.
    static AFTER_INCREMENT_DETECT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);

    /// Deterministic filesystem-race seam after the authoritative graph parse.
    static AFTER_INCREMENT_PARSE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);

    /// Deterministic inter-statement publication seam for reservation tests.
    static AFTER_INCREMENT_FINGERPRINT_CHECK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
fn run_after_increment_detect_hook() {
    AFTER_INCREMENT_DETECT.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(test)]
fn install_after_increment_detect_hook(hook: impl FnOnce() + 'static) {
    AFTER_INCREMENT_DETECT.with(|slot| {
        let previous = slot.borrow_mut().replace(Box::new(hook));
        assert!(
            previous.is_none(),
            "increment detect test hook already installed"
        );
    });
}

#[cfg(test)]
fn run_after_increment_parse_hook() {
    AFTER_INCREMENT_PARSE.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(test)]
fn install_after_increment_parse_hook(hook: impl FnOnce() + 'static) {
    AFTER_INCREMENT_PARSE.with(|slot| {
        let previous = slot.borrow_mut().replace(Box::new(hook));
        assert!(
            previous.is_none(),
            "increment parse test hook already installed"
        );
    });
}

#[cfg(test)]
fn run_after_increment_fingerprint_check_hook() {
    AFTER_INCREMENT_FINGERPRINT_CHECK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(test)]
fn install_after_increment_fingerprint_check_hook(hook: impl FnOnce() + 'static) {
    AFTER_INCREMENT_FINGERPRINT_CHECK.with(|slot| {
        let previous = slot.borrow_mut().replace(Box::new(hook));
        assert!(
            previous.is_none(),
            "increment fingerprint-check test hook already installed"
        );
    });
}

// Superseded by `ChangeDetectOptions` (the live force-hash knob). Kept to
// preserve the writer's option-struct shape; safe to delete in a cleanup pass.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct IndexOptions {
    pub force_hash: bool,
}

#[derive(Debug, Clone, Default)]
pub struct IndexReport {
    pub doc_count: usize,
    pub link_count: usize,
    pub file_count: usize,
    pub duration_ms: u128,
}

impl crate::cache::Cache {
    /// Returns true if a full rebuild has ever stamped this cache (a
    /// `last_full_rebuild_ts` meta row exists). Fresh caches and caches that
    /// have only seen schema/meta init return false.
    ///
    /// `pub(crate)` so the NRN-253 freshness probe can report an unbuilt cache as
    /// Stale (routing it through the refresh/rebuild path), mirroring how
    /// `index_incremental` defers to `rebuild` in exactly that case.
    pub(crate) fn has_been_built(&self) -> Result<bool, CacheError> {
        let row: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'last_full_rebuild_ts'",
                [],
                |r| r.get(0),
            )
            .ok();
        Ok(row.is_some())
    }

    /// Full rebuild: walk the vault, parse every document, replace all rows.
    /// Used by `norn cache rebuild` and the implicit rebuild after a self-heal trigger.
    pub fn rebuild(&mut self, vault_root: &Utf8Path) -> Result<IndexReport, CacheError> {
        let _lock = crate::cache::lock::WriteLock::acquire(
            &self.cache_dir,
            crate::cache::lock::write_lock_timeout(),
        )?;
        let start = std::time::Instant::now();
        let options = crate::graph::IndexOptions {
            ignore: self.files_ignore.clone(),
            alias_field: self.alias_field.clone(),
            ..Default::default()
        };
        let index = crate::graph::build_index_with_options(vault_root, &options)?;

        let tx = self.conn.transaction()?;
        clear_all_rows(&tx)?;
        let mut report = IndexReport::default();
        for doc in &index.documents {
            insert_document(&tx, vault_root, doc, &mut report, &self.index_set)?;
        }
        for file in &index.files {
            insert_file(&tx, vault_root, file)?;
            report.file_count += 1;
        }
        update_meta_graph_fingerprint(&tx, &graph_fingerprint(&index))?;
        update_meta_rebuild_ts(&tx)?;
        update_meta_alias_field(&tx, self.alias_field.as_deref())?;
        // Only an authoritative open (`Cache::open_with_index`) knows the
        // operator's real declared index set — stamping the hash from a
        // non-authoritative open's unconfigured-default empty set would
        // make a later authoritative open think it's already reconciled.
        // Leave whatever stamp already exists; that later open reconciles.
        if self.index_authoritative {
            update_meta_index_set_hash(&tx, &self.index_set_hash)?;
        }
        tx.commit()?;
        // Refresh query-planner statistics for `document_fields` after a full
        // rewrite; deliberately not run on the per-doc incremental path
        // below, where the row-count delta is too small to matter. Scoped to
        // this one table (rather than a schema-wide `ANALYZE`) so it doesn't
        // perturb existing planner decisions for `links`/`documents` on
        // small test fixtures — see the EXPLAIN-guard tests in query.rs /
        // query_show.rs.
        self.conn.execute("ANALYZE document_fields", [])?;

        report.duration_ms = start.elapsed().as_millis();
        Ok(report)
    }

    /// Incremental update: detect changes against the cached state, then
    /// drop+reinsert only the affected documents. Re-runs the full
    /// `crate::graph::build_index` for parse authority but updates only the
    /// changed-document subset of rows.
    ///
    /// When the cache has never been fully built (no `last_full_rebuild_ts`
    /// meta row), this defers to `rebuild` so attachments and other non-Markdown
    /// files are populated — the cheap change-detector only walks `.md` files.
    pub fn index_incremental(
        &mut self,
        vault_root: &Utf8Path,
        options: &ChangeDetectOptions,
    ) -> Result<IndexReport, CacheError> {
        if !self.has_been_built()? {
            return self.rebuild(vault_root);
        }
        let _lock = crate::cache::lock::WriteLock::acquire(
            &self.cache_dir,
            crate::cache::lock::write_lock_timeout(),
        )?;
        let start = std::time::Instant::now();
        let mut changes = detect(vault_root, self, options)?;
        if changes.is_empty() {
            return Ok(IndexReport::default());
        }
        #[cfg(test)]
        run_after_increment_detect_hook();

        // Re-parse the affected docs from the filesystem. Aggressive
        // invalidation: re-run build_index on the whole vault and pick out
        // the affected documents. Simpler than scoped parsing, and the
        // per-doc cost dominates only on truly huge vaults where
        // parse-everything beats incremental in total time anyway.
        let options = crate::graph::IndexOptions {
            ignore: self.files_ignore.clone(),
            alias_field: self.alias_field.clone(),
            ..Default::default()
        };
        let mut fresh_index = crate::graph::build_index_with_options(vault_root, &options)?;
        #[cfg(test)]
        run_after_increment_parse_hook();

        // Detection and the authoritative whole-vault parse are separate
        // filesystem observations. Expand the affected set to include every
        // path+hash difference the parse observed, so an unrelated document
        // changed between those observations cannot leave document rows from
        // the old graph beside files, links, and a fingerprint from the new
        // graph. Keep the initially detected paths too: their metadata may
        // need refreshing even if their bytes were restored before parsing.
        let cached_hashes = load_document_hashes(&self.conn)?;
        let fresh_hashes: std::collections::BTreeMap<_, _> = fresh_index
            .documents
            .iter()
            .map(|document| (document.path.clone(), document.hash.clone()))
            .collect();
        let mut affected_paths: std::collections::BTreeSet<_> = changes
            .iter()
            .map(|change| match change {
                FileChange::Added(path)
                | FileChange::Modified(path)
                | FileChange::Deleted(path) => path.clone(),
            })
            .collect();
        let all_document_paths: std::collections::BTreeSet<_> = cached_hashes
            .keys()
            .chain(fresh_hashes.keys())
            .cloned()
            .collect();
        for path in all_document_paths {
            if affected_paths.contains(&path) {
                continue;
            }
            let drift = match (cached_hashes.get(&path), fresh_hashes.get(&path)) {
                (None, Some(_)) => Some(FileChange::Added(path.clone())),
                (Some(_), None) => Some(FileChange::Deleted(path.clone())),
                (Some(cached), Some(fresh)) if cached != fresh => {
                    Some(FileChange::Modified(path.clone()))
                }
                _ => None,
            };
            if let Some(drift) = drift {
                affected_paths.insert(path);
                changes.push(drift);
            }
        }
        fn change_path(change: &FileChange) -> &Utf8Path {
            match change {
                FileChange::Added(path)
                | FileChange::Modified(path)
                | FileChange::Deleted(path) => path,
            }
        }
        changes.sort_by(|a, b| change_path(a).cmp(change_path(b)));

        // Incremental detection is authoritative only for Markdown paths. Keep
        // cached file identity everywhere else, then overlay the affected
        // Markdown paths from this parse. Unrelated attachment disk races must
        // not enter a Markdown refresh; explicit mutation increments own those
        // paths. Resolve every link against the exact overlay that will be
        // persisted and fingerprinted below.
        let mut overlay_files = crate::cache::reader::load_files(&self.conn)?;
        overlay_files.retain(|file| !affected_paths.contains(&file.path));
        overlay_files.extend(
            fresh_index
                .files
                .iter()
                .filter(|file| affected_paths.contains(&file.path))
                .cloned(),
        );
        overlay_files.sort_by(|a, b| a.path.cmp(&b.path));
        fresh_index.files = overlay_files;
        crate::links::resolve_links(&fresh_index.files, &mut fresh_index.documents);

        let fresh_docs: std::collections::HashMap<_, _> = fresh_index
            .documents
            .iter()
            .map(|d| (d.path.clone(), d))
            .collect();
        let fresh_files: std::collections::HashMap<_, _> = fresh_index
            .files
            .iter()
            .map(|file| (file.path.clone(), file))
            .collect();

        // The parse is authoritative for affected Markdown content, so its
        // metadata must describe those same parsed bytes. A path can drift
        // after `build_index_with_options` returns; publishing later live
        // metadata would otherwise bless unparsed bytes as fresh. Validate and
        // capture metadata before opening the publication transaction. On
        // drift, retain the previous coherent snapshot so a later refresh can
        // detect and heal it.
        let mut parsed_metadata = std::collections::HashMap::new();
        for path in &affected_paths {
            if let Some(doc) = fresh_docs.get(path) {
                let metadata = metadata_for_parsed_bytes(vault_root, doc)
                    .ok_or_else(|| CacheError::IncrementSourceDrift { path: path.clone() })?;
                parsed_metadata.insert(path.clone(), metadata);
            } else if !graph_intentionally_ignores(path, &self.files_ignore) {
                match std::fs::symlink_metadata(vault_root.join(path).as_std_path()) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Ok(_) | Err(_) => {
                        return Err(CacheError::IncrementSourceDrift { path: path.clone() });
                    }
                }
            }
        }

        let tx = self.conn.transaction()?;
        let mut report = IndexReport::default();

        for change in &changes {
            let path = change_path(change);
            tx.execute("DELETE FROM files WHERE path = ?", [path.as_str()])?;
            if let Some(file) = fresh_files.get(path) {
                let &(mtime_ns, size_bytes) =
                    parsed_metadata
                        .get(path)
                        .ok_or_else(|| CacheError::IncrementSourceDrift {
                            path: path.to_owned(),
                        })?;
                insert_file_with_metadata(&tx, file, mtime_ns, size_bytes)?;
            }
            crate::cache::invalidation::drop_document(&tx, path)?;
            if let Some(doc) = fresh_docs.get(path) {
                let &(mtime_ns, size_bytes) =
                    parsed_metadata
                        .get(path)
                        .ok_or_else(|| CacheError::IncrementSourceDrift {
                            path: path.to_owned(),
                        })?;
                insert_document_with_metadata(
                    &tx,
                    doc,
                    &mut report,
                    &self.index_set,
                    mtime_ns,
                    size_bytes,
                )?;
            }
        }

        // Rewrite the entire links table from the fresh index. Link resolution is
        // global, so this is the step that keeps an incremental refresh identical
        // to a full rebuild — the per-doc invalidation above updates doc rows;
        // link resolution is not decomposable per-doc (NRN-126). This supersedes
        // any incoming-link fixup, so no `unresolve_incoming` is needed above.
        rerun_link_resolution(&tx, &fresh_index)?;
        update_meta_graph_fingerprint(&tx, &graph_fingerprint(&fresh_index))?;

        tx.commit()?;

        report.duration_ms = start.elapsed().as_millis();
        Ok(report)
    }
}

/// The wall-time budget one increment-commit chunk aims to stay within before
/// yielding to the writer queue's liveness class (ADR 0013 Phase 2, NRN-252,
/// NRN-158). A chunk always stages at least one WHOLE file or link, then stops once
/// this budget is spent — so a chunk never splits a document's rows and preemption
/// latency stays bounded to roughly one chunk. 50ms in production.
pub(crate) const INCREMENT_CHUNK_BUDGET: Duration = Duration::from_millis(50);

/// Test-only override for [`INCREMENT_CHUNK_BUDGET`], in milliseconds. DEBUG
/// BUILDS ONLY — the env read is compiled out of release builds (via the shared
/// [`crate::cache::lock::debug_env_duration_ms`], exactly like
/// [`crate::cache::lock::write_lock_timeout`]'s `NORN_CACHE_LOCK_TIMEOUT_MS`), so
/// no production environment can shrink the budget. A tiny value (including `0`)
/// forces one-file-or-link-per-chunk so tests can observe staging boundaries
/// deterministically.
const INCREMENT_CHUNK_BUDGET_ENV: &str = "NORN_CACHE_INCREMENT_BUDGET_MS";

/// The increment-commit chunk budget (see [`INCREMENT_CHUNK_BUDGET`]).
///
/// **Release builds always return the 50ms const** — the env read is compiled
/// out. Debug builds honor `NORN_CACHE_INCREMENT_BUDGET_MS` when it parses to an
/// integer; unlike the write-lock timeout this ACCEPTS `0` (yield after every
/// single file), so it passes `accept_zero = true` to the shared reader.
pub(crate) fn increment_chunk_budget() -> Duration {
    crate::cache::lock::debug_env_duration_ms(
        INCREMENT_CHUNK_BUDGET_ENV,
        INCREMENT_CHUNK_BUDGET,
        /* accept_zero = */ true,
    )
}

/// The phase of a chunked [`IncrementCommit`]. Document-owned rows stage first,
/// then the complete globally-resolved links set. `Ready` provides the mandatory
/// liveness boundary immediately before one short atomic main publication.
enum IncrementPhase {
    /// Stage whole changed paths and their document-owned rows in TEMP.
    Files,
    /// Stage the complete globally-resolved links set in TEMP.
    Links,
    /// All staging is complete; the next entry performs one atomic publication.
    Ready,
    /// Main was published atomically.
    Done,
    /// A newer publication superseded this staged job.
    Superseded,
}

static NEXT_INCREMENT_JOB_ID: AtomicU64 = AtomicU64::new(1);
const INCREMENT_STAGING_TABLES: &[&str] = &[
    "norn_increment_jobs",
    "norn_increment_links",
    "norn_increment_diagnostics",
    "norn_increment_block_ids",
    "norn_increment_headings",
    "norn_increment_document_fields",
    "norn_increment_documents",
    "norn_increment_files",
    "norn_increment_paths",
];

/// Driver state for a chunked increment commit (ADR 0013 Phase 2, NRN-252,
/// NRN-158). Built once at op start by [`Cache::begin_increment_commit`] — which
/// performs the whole-vault parse WITHOUT holding the WriteLock — then advanced
/// one chunk at a time by [`Cache::commit_increment_chunk`].
///
/// `fresh_index` is an authority overlay: generation N supplies every unrelated
/// document/file while the post-mutation disk parse supplies only affected
/// paths. Links are then re-resolved over that composed graph. This prevents an
/// unrelated concurrent disk edit from entering or invalidating an increment.
pub(crate) struct IncrementCommit {
    job_id: i64,
    publication_epoch: u64,
    fresh_index: GraphIndex,
    fresh_docs: HashMap<Utf8PathBuf, usize>,
    fresh_files: HashMap<Utf8PathBuf, usize>,
    affected_paths: std::collections::BTreeSet<Utf8PathBuf>,
    publication_fingerprint: String,
    intentionally_ignored_paths: std::collections::BTreeSet<Utf8PathBuf>,
    parsed_metadata: HashMap<Utf8PathBuf, (i64, i64)>,
    file_metadata: HashMap<Utf8PathBuf, (i64, i64)>,
    pending: VecDeque<Utf8PathBuf>,
    pending_links: VecDeque<(usize, usize)>,
    next_link_sequence: i64,
    phase: IncrementPhase,
}

/// Publication authority reserved on the dedicated writer connection before
/// the request thread parses the vault. Its TEMP job marker carries the external
/// publication guard and lets a same-connection refresh supersede the work even
/// before the first bulk chunk is submitted.
#[derive(Debug)]
pub(crate) struct IncrementReservation {
    job_id: i64,
    publication_epoch: u64,
}

impl crate::cache::Cache {
    /// Begin a chunked increment commit for an explicit set of changed vault-
    /// relative paths, WITHOUT a detect scan (NRN-158). Performs the whole-vault
    /// `build_index_with_options` parse in memory — the same "aggressive
    /// invalidation" authority `index_incremental` uses — but holds NO WriteLock
    /// during the parse (scoped parsing is out of scope, tracked as NRN-154).
    ///
    /// An ASSOCIATED function (no `&self`): the parse is read-only over the
    /// filesystem and needs only the operator's `alias_field` / `files_ignore`
    /// config, NOT a cache connection. Taking the options explicitly lets the
    /// warm daemon build the [`IncrementCommit`] on the REQUEST thread (off the
    /// writer thread) so the O(parse) whole-vault walk never runs as one
    /// unbounded, non-preemptible writer-queue chunk (NRN-252 review). The
    /// returned commit is then driven by
    /// [`commit_increment_chunk`](Self::commit_increment_chunk); no rows are
    /// written here.
    pub(crate) fn begin_increment_commit(
        vault_root: &Utf8Path,
        changed_paths: &[Utf8PathBuf],
        alias_field: Option<&str>,
        files_ignore: &[String],
        reservation: &IncrementReservation,
        mut baseline: GraphIndex,
    ) -> Result<IncrementCommit, CacheError> {
        let options = crate::graph::IndexOptions {
            ignore: files_ignore.to_vec(),
            alias_field: alias_field.map(|s| s.to_string()),
            ..Default::default()
        };
        let disk_index = crate::graph::build_index_with_options(vault_root, &options)?;

        // Sort + dedup so chunk boundaries are deterministic and a path is never
        // staged twice.
        let mut pending: Vec<Utf8PathBuf> = changed_paths.to_vec();
        pending.sort();
        pending.dedup();
        let affected_paths: std::collections::BTreeSet<_> = pending.iter().cloned().collect();
        let intentionally_ignored_paths = affected_paths
            .iter()
            .filter(|path| graph_intentionally_ignores(path, files_ignore))
            .cloned()
            .collect();

        // Generation N is authoritative for everything outside this mutation.
        // Only affected paths cross from the post-mutation disk parse into the
        // overlay; ignored destinations and deletions are therefore represented
        // by absence without consulting their current filesystem existence.
        baseline
            .documents
            .retain(|doc| !affected_paths.contains(&doc.path));
        baseline
            .files
            .retain(|file| !affected_paths.contains(&file.path));
        baseline.documents.extend(
            disk_index
                .documents
                .iter()
                .filter(|doc| affected_paths.contains(&doc.path))
                .cloned(),
        );
        baseline.files.extend(
            disk_index
                .files
                .iter()
                .filter(|file| affected_paths.contains(&file.path))
                .cloned(),
        );
        baseline.documents.sort_by(|a, b| a.path.cmp(&b.path));
        baseline.files.sort_by(|a, b| a.path.cmp(&b.path));
        crate::links::resolve_links(&baseline.files, &mut baseline.documents);
        let fresh_index = baseline;
        let publication_fingerprint = graph_fingerprint(&fresh_index);
        let fresh_docs: HashMap<Utf8PathBuf, usize> = fresh_index
            .documents
            .iter()
            .enumerate()
            .map(|(i, d)| (d.path.clone(), i))
            .collect();
        let fresh_files: HashMap<Utf8PathBuf, usize> = fresh_index
            .files
            .iter()
            .enumerate()
            .map(|(i, file)| (file.path.clone(), i))
            .collect();
        let parsed_metadata: HashMap<Utf8PathBuf, (i64, i64)> = affected_paths
            .iter()
            .filter_map(|path| {
                let &index = fresh_docs.get(path)?;
                metadata_for_parsed_bytes(vault_root, &fresh_index.documents[index])
                    .map(|metadata| (path.clone(), metadata))
            })
            .collect();
        let file_metadata = affected_paths
            .iter()
            .filter_map(|path| {
                fresh_files.get(path)?;
                parsed_metadata
                    .get(path)
                    .copied()
                    .or_else(|| stable_file_metadata(vault_root, path))
                    .map(|metadata| (path.clone(), metadata))
            })
            .collect();
        let pending_links = fresh_index
            .documents
            .iter()
            .enumerate()
            .flat_map(|(doc_i, doc)| (0..doc.links.len()).map(move |link_i| (doc_i, link_i)))
            .collect();
        Ok(IncrementCommit {
            job_id: reservation.job_id,
            publication_epoch: reservation.publication_epoch,
            fresh_index,
            fresh_docs,
            fresh_files,
            affected_paths,
            publication_fingerprint,
            intentionally_ignored_paths,
            parsed_metadata,
            file_metadata,
            pending: pending.into_iter().collect(),
            pending_links,
            next_link_sequence: 0,
            phase: IncrementPhase::Files,
        })
    }

    /// Reserve a job marker and its external-publication baseline on the
    /// dedicated writer connection. `data_version` is captured before the O(1)
    /// graph-fingerprint check: a publication before the check changes the
    /// fingerprint, while one after it changes the terminal data version. The
    /// warm path submits and awaits this as a short liveness op before doing the
    /// whole-vault parse off-thread.
    pub(crate) fn reserve_increment_commit(
        &mut self,
        expected_fingerprint: &str,
    ) -> Result<IncrementReservation, CacheError> {
        ensure_increment_staging_tables(&self.conn)?;
        let base_data_version: i64 =
            self.conn
                .query_row("PRAGMA main.data_version", [], |row| row.get(0))?;
        let stored_fingerprint = self.conn.query_row(
            "SELECT value FROM main.meta WHERE key = 'graph_fingerprint'",
            [],
            |row| row.get::<_, String>(0),
        );
        match stored_fingerprint {
            Ok(stored) if stored == expected_fingerprint => {}
            Ok(_) | Err(rusqlite::Error::QueryReturnedNoRows) => {
                return Err(CacheError::IncrementBaselineDrift);
            }
            Err(error) => return Err(error.into()),
        }
        #[cfg(test)]
        run_after_increment_fingerprint_check_hook();
        let job_id = NEXT_INCREMENT_JOB_ID.fetch_add(1, Ordering::Relaxed) as i64;
        self.conn.execute(
            "INSERT INTO temp.norn_increment_jobs
             (job_id, base_data_version, publication_epoch) VALUES (?, ?, ?)",
            params![
                job_id,
                base_data_version,
                self.increment_publication_epoch as i64
            ],
        )?;
        Ok(IncrementReservation {
            job_id,
            publication_epoch: self.increment_publication_epoch,
        })
    }

    /// Best-effort reservation cleanup used when parsing fails before a bulk
    /// driver exists. Marker invalidation happens before payload cleanup.
    pub(crate) fn discard_increment_reservation(
        &mut self,
        reservation: &IncrementReservation,
    ) -> Result<(), CacheError> {
        self.cleanup_staged_increment(reservation.job_id)
    }

    /// Advance ONE chunk of an increment. Bulk chunks write only job-scoped TEMP
    /// rows on this dedicated connection and return `Ok(true)` so the scheduler
    /// can run liveness work between every chunk. The terminal entry alone takes
    /// the WriteLock and atomically replaces affected main rows plus all links.
    /// Returns `Ok(false)` after publication or supersession.
    ///
    /// - **Files phase:** stage WHOLE files until the wall-time
    ///   `budget` is spent (always at least one file, so a chunk never splits a
    ///   document's rows). Disk is truth: a path present in the parse gets staged;
    ///   an absent path is represented only by the affected-path marker.
    /// - **Links phase:** stage the complete global links set from the parse. Link
    ///   resolution is a global function of the whole document set (NRN-126), so
    ///   this is what makes the published increment identical to a full rebuild
    ///   (an alias/path/stem change re-resolves links in unchanged files too).
    pub(crate) fn commit_increment_chunk(
        &mut self,
        _vault_root: &Utf8Path,
        commit: &mut IncrementCommit,
        budget: Duration,
    ) -> Result<bool, CacheError> {
        if commit.publication_epoch != self.increment_publication_epoch
            || !self.staged_increment_exists(commit.job_id)?
        {
            let _ = self.cleanup_staged_increment(commit.job_id);
            commit.phase = IncrementPhase::Superseded;
            return Ok(false);
        }

        let result = match commit.phase {
            IncrementPhase::Files => self.stage_increment_files(commit, budget),
            IncrementPhase::Links => self.stage_increment_links(commit, budget),
            IncrementPhase::Ready => self.publish_increment(commit).map(|()| false),
            IncrementPhase::Done | IncrementPhase::Superseded => Ok(false),
        };
        if result.is_err() {
            let _ = self.cleanup_staged_increment(commit.job_id);
        }
        result
    }

    /// Discard every currently-staged increment on this connection. A successful
    /// liveness refresh calls this after publishing its newer main snapshot; old
    /// bulk drivers then observe their missing job marker and finish without
    /// publishing. Failed/contended refreshes deliberately do not call it.
    pub(crate) fn supersede_staged_increments_after_refresh(&mut self) {
        // Non-fallible revocation happens before any TEMP cleanup. Even if a
        // trigger/SQLite error blocks DELETE, old drivers fail the epoch check.
        self.increment_publication_epoch = self.increment_publication_epoch.saturating_add(1);
        let _ = ensure_increment_staging_tables(&self.conn);
        for table in INCREMENT_STAGING_TABLES {
            let _ = self.conn.execute(&format!("DELETE FROM temp.{table}"), []);
        }
    }

    fn staged_increment_exists(&self, job_id: i64) -> Result<bool, CacheError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM temp.norn_increment_jobs WHERE job_id = ?",
            [job_id],
            |row| row.get(0),
        )?;
        Ok(count == 1)
    }

    fn stage_increment_files(
        &mut self,
        commit: &mut IncrementCommit,
        budget: Duration,
    ) -> Result<bool, CacheError> {
        if commit.pending.is_empty() {
            commit.phase = IncrementPhase::Links;
            return self.stage_increment_links(commit, budget);
        }
        let tx = self.conn.transaction()?;
        let start = Instant::now();
        while let Some(path) = commit.pending.pop_front() {
            tx.execute(
                "INSERT INTO temp.norn_increment_paths (job_id, path) VALUES (?, ?)",
                params![commit.job_id, path.as_str()],
            )?;
            if let Some(&i) = commit.fresh_docs.get(&path) {
                let Some(&(mtime_ns, size_bytes)) = commit.parsed_metadata.get(&path) else {
                    tx.rollback()?;
                    let _ = self.cleanup_staged_increment(commit.job_id);
                    return Err(CacheError::IncrementSourceDrift { path });
                };
                stage_document(
                    &tx,
                    commit.job_id,
                    &commit.fresh_index.documents[i],
                    &self.index_set,
                    mtime_ns,
                    size_bytes,
                )?;
            }
            if let Some(&i) = commit.fresh_files.get(&path) {
                let Some(&(mtime_ns, size_bytes)) = commit.file_metadata.get(&path) else {
                    tx.rollback()?;
                    let _ = self.cleanup_staged_increment(commit.job_id);
                    return Err(CacheError::IncrementSourceDrift { path });
                };
                stage_file(
                    &tx,
                    commit.job_id,
                    &commit.fresh_index.files[i],
                    mtime_ns,
                    size_bytes,
                )?;
            }
            if start.elapsed() >= budget {
                break;
            }
        }
        tx.commit()?;
        if commit.pending.is_empty() {
            commit.phase = IncrementPhase::Links;
        }
        Ok(true)
    }

    fn stage_increment_links(
        &mut self,
        commit: &mut IncrementCommit,
        budget: Duration,
    ) -> Result<bool, CacheError> {
        if commit.pending_links.is_empty() {
            commit.phase = IncrementPhase::Ready;
            return Ok(true);
        }
        let tx = self.conn.transaction()?;
        let start = Instant::now();
        while let Some((doc_i, link_i)) = commit.pending_links.pop_front() {
            stage_link(
                &tx,
                commit.job_id,
                commit.next_link_sequence,
                &commit.fresh_index.documents[doc_i].links[link_i],
            )?;
            commit.next_link_sequence += 1;
            if start.elapsed() >= budget {
                break;
            }
        }
        tx.commit()?;
        if commit.pending_links.is_empty() {
            commit.phase = IncrementPhase::Ready;
        }
        Ok(true)
    }

    fn publish_increment(&mut self, commit: &mut IncrementCommit) -> Result<(), CacheError> {
        let _lock = crate::cache::lock::WriteLock::acquire(
            &self.cache_dir,
            crate::cache::lock::write_lock_timeout(),
        )?;
        if !self.staged_increment_exists(commit.job_id)? {
            commit.phase = IncrementPhase::Superseded;
            return Ok(());
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let job_id = commit.job_id;
        if commit.publication_epoch != self.increment_publication_epoch {
            tx.rollback()?;
            let _ = self.cleanup_staged_increment(job_id);
            commit.phase = IncrementPhase::Superseded;
            return Ok(());
        }
        let base_data_version: i64 = tx.query_row(
            "SELECT base_data_version FROM temp.norn_increment_jobs WHERE job_id = ?",
            [job_id],
            |row| row.get(0),
        )?;
        let current_data_version: i64 =
            tx.query_row("PRAGMA main.data_version", [], |row| row.get(0))?;
        if current_data_version != base_data_version {
            tx.rollback()?;
            self.cleanup_staged_increment(job_id)?;
            commit.phase = IncrementPhase::Superseded;
            return Ok(());
        }
        // Validate source bytes only after publication authority. An intervening
        // cache publication supersedes this job cleanly; otherwise this is the
        // last filesystem read before the atomic main-row replacement.
        if let Err(error) = affected_sources_still_match(&self.vault_root, commit) {
            tx.rollback()?;
            return Err(error);
        }
        tx.execute("DELETE FROM main.links", [])?;
        for table in ["diagnostics", "block_ids", "headings", "document_fields"] {
            tx.execute(
                &format!(
                    "DELETE FROM main.{table} WHERE {} IN \
                     (SELECT path FROM temp.norn_increment_paths WHERE job_id = ?)",
                    if table == "document_fields" {
                        "path"
                    } else {
                        "doc_path"
                    }
                ),
                [job_id],
            )?;
        }
        tx.execute(
            "DELETE FROM main.documents WHERE path IN \
             (SELECT path FROM temp.norn_increment_paths WHERE job_id = ?)",
            [job_id],
        )?;
        tx.execute(
            "DELETE FROM main.files WHERE path IN \
             (SELECT path FROM temp.norn_increment_paths WHERE job_id = ?)",
            [job_id],
        )?;
        tx.execute(
            "INSERT INTO main.files (path, ext, size_bytes, mtime_ns) \
             SELECT path, ext, size_bytes, mtime_ns FROM temp.norn_increment_files \
             WHERE job_id = ? ORDER BY path",
            [job_id],
        )?;
        tx.execute(
            "INSERT INTO main.documents \
             (path, stem, hash, frontmatter_json, body_text, mtime_ns, size_bytes) \
             SELECT path, stem, hash, frontmatter_json, body_text, mtime_ns, size_bytes \
             FROM temp.norn_increment_documents WHERE job_id = ? ORDER BY path",
            [job_id],
        )?;
        tx.execute(
            "INSERT INTO main.document_fields (path, key, value) \
             SELECT path, key, value FROM temp.norn_increment_document_fields \
             WHERE job_id = ? ORDER BY path, key, sequence",
            [job_id],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO main.headings \
             (doc_path, level, text, slug, source_span_line, source_span_column, source_span_byte_offset) \
             SELECT doc_path, level, text, slug, source_span_line, source_span_column, source_span_byte_offset \
             FROM temp.norn_increment_headings WHERE job_id = ? ORDER BY doc_path, sequence",
            [job_id],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO main.block_ids (doc_path, block_id) \
             SELECT doc_path, block_id FROM temp.norn_increment_block_ids \
             WHERE job_id = ? ORDER BY doc_path, sequence",
            [job_id],
        )?;
        tx.execute(
            "INSERT INTO main.diagnostics (doc_path, severity, code, message, detail) \
             SELECT doc_path, severity, code, message, detail \
             FROM temp.norn_increment_diagnostics WHERE job_id = ? ORDER BY doc_path, sequence",
            [job_id],
        )?;
        tx.execute(
            "INSERT INTO main.links \
             (source_path, raw, kind, target_raw, resolved_path, anchor, block_ref, label, \
              source_span_start, source_span_end, source_span_line, source_span_column, \
              source_context, source_context_property, status, unresolved_reason, candidates_json) \
             SELECT source_path, raw, kind, target_raw, resolved_path, anchor, block_ref, label, \
                    source_span_start, source_span_end, source_span_line, source_span_column, \
                    source_context, source_context_property, status, unresolved_reason, candidates_json \
             FROM temp.norn_increment_links WHERE job_id = ? ORDER BY sequence",
            [job_id],
        )?;
        update_meta_graph_fingerprint(&tx, &commit.publication_fingerprint)?;
        tx.commit()?;
        // This publication supersedes every other reservation captured before
        // it, including jobs that have parsed but not entered their first chunk.
        self.increment_publication_epoch = self.increment_publication_epoch.saturating_add(1);
        let _ = self.cleanup_staged_increment(job_id);
        commit.phase = IncrementPhase::Done;
        Ok(())
    }

    fn cleanup_staged_increment(&self, job_id: i64) -> Result<(), CacheError> {
        for table in INCREMENT_STAGING_TABLES {
            self.conn.execute(
                &format!("DELETE FROM temp.{table} WHERE job_id = ?"),
                [job_id],
            )?;
        }
        Ok(())
    }

    /// Test-only: how many changes a `detect` scan reports for `vault_root`
    /// against this connection's cached state. Drives the NRN-158 assertion that
    /// a committed increment leaves the next refresh with zero work.
    #[cfg(test)]
    pub(crate) fn detect_change_count(&self, vault_root: &Utf8Path) -> usize {
        detect(vault_root, self, &ChangeDetectOptions::default())
            .expect("detect")
            .len()
    }

    #[cfg(test)]
    pub(crate) fn staged_increment_job_count(&self) -> i64 {
        self.conn
            .query_row("SELECT COUNT(*) FROM temp.norn_increment_jobs", [], |row| {
                row.get(0)
            })
            .unwrap_or(0)
    }
}

fn ensure_increment_staging_tables(conn: &rusqlite::Connection) -> Result<(), CacheError> {
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS norn_increment_jobs (
             job_id INTEGER PRIMARY KEY,
             base_data_version INTEGER NOT NULL,
             publication_epoch INTEGER NOT NULL
         );
         CREATE TEMP TABLE IF NOT EXISTS norn_increment_paths (
             job_id INTEGER NOT NULL,
             path TEXT NOT NULL,
             PRIMARY KEY (job_id, path)
         );
         CREATE TEMP TABLE IF NOT EXISTS norn_increment_files (
             job_id INTEGER NOT NULL,
             path TEXT NOT NULL,
             ext TEXT NOT NULL,
             size_bytes INTEGER NOT NULL,
             mtime_ns INTEGER NOT NULL,
             PRIMARY KEY (job_id, path)
         );
         CREATE TEMP TABLE IF NOT EXISTS norn_increment_documents (
             job_id INTEGER NOT NULL,
             path TEXT NOT NULL,
             stem TEXT NOT NULL,
             hash TEXT NOT NULL,
             frontmatter_json TEXT,
             body_text TEXT NOT NULL,
             mtime_ns INTEGER NOT NULL,
             size_bytes INTEGER NOT NULL,
             PRIMARY KEY (job_id, path)
         );
         CREATE TEMP TABLE IF NOT EXISTS norn_increment_document_fields (
             job_id INTEGER NOT NULL,
             path TEXT NOT NULL,
             key TEXT NOT NULL,
             value,
             sequence INTEGER NOT NULL,
             PRIMARY KEY (job_id, path, key, sequence)
         );
         CREATE TEMP TABLE IF NOT EXISTS norn_increment_headings (
             job_id INTEGER NOT NULL,
             doc_path TEXT NOT NULL,
             level INTEGER NOT NULL,
             text TEXT NOT NULL,
             slug TEXT NOT NULL,
             source_span_line INTEGER,
             source_span_column INTEGER,
             source_span_byte_offset INTEGER,
             sequence INTEGER NOT NULL,
             PRIMARY KEY (job_id, doc_path, sequence)
         );
         CREATE TEMP TABLE IF NOT EXISTS norn_increment_block_ids (
             job_id INTEGER NOT NULL,
             doc_path TEXT NOT NULL,
             block_id TEXT NOT NULL,
             sequence INTEGER NOT NULL,
             PRIMARY KEY (job_id, doc_path, sequence)
         );
         CREATE TEMP TABLE IF NOT EXISTS norn_increment_diagnostics (
             job_id INTEGER NOT NULL,
             doc_path TEXT NOT NULL,
             severity TEXT NOT NULL,
             code TEXT NOT NULL,
             message TEXT NOT NULL,
             detail TEXT,
             sequence INTEGER NOT NULL,
             PRIMARY KEY (job_id, doc_path, sequence)
         );
         CREATE TEMP TABLE IF NOT EXISTS norn_increment_links (
             job_id INTEGER NOT NULL,
             sequence INTEGER NOT NULL,
             source_path TEXT NOT NULL,
             raw TEXT NOT NULL,
             kind TEXT NOT NULL,
             target_raw TEXT NOT NULL,
             resolved_path TEXT,
             anchor TEXT,
             block_ref TEXT,
             label TEXT,
             source_span_start INTEGER,
             source_span_end INTEGER,
             source_span_line INTEGER,
             source_span_column INTEGER,
             source_context TEXT,
             source_context_property TEXT,
             status TEXT NOT NULL,
             unresolved_reason TEXT,
             candidates_json TEXT,
             PRIMARY KEY (job_id, sequence)
         );",
    )?;
    Ok(())
}

fn stage_document(
    tx: &Transaction,
    job_id: i64,
    doc: &Document,
    index_set: &std::collections::BTreeSet<String>,
    parsed_mtime_ns: i64,
    parsed_size_bytes: i64,
) -> Result<(), CacheError> {
    let frontmatter_json = doc
        .frontmatter
        .as_ref()
        .map(|value| serde_json::to_string(value).unwrap_or_default());
    tx.execute(
        "INSERT INTO temp.norn_increment_documents
         (job_id, path, stem, hash, frontmatter_json, body_text, mtime_ns, size_bytes)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            job_id,
            doc.path.as_str(),
            doc.stem,
            doc.hash,
            frontmatter_json,
            doc.body_text,
            parsed_mtime_ns,
            parsed_size_bytes,
        ],
    )?;

    let mut sequence = 0_i64;
    crate::cache::document_fields::visit_expanded_rows(
        doc.frontmatter.as_ref(),
        index_set,
        |key, value| -> Result<(), CacheError> {
            tx.execute(
                "INSERT INTO temp.norn_increment_document_fields
             (job_id, path, key, value, sequence) VALUES (?, ?, ?, ?, ?)",
                params![job_id, doc.path.as_str(), key, value, sequence],
            )?;
            sequence += 1;
            Ok(())
        },
    )?;

    for (sequence, heading) in doc.headings.iter().enumerate() {
        let (line, column, byte_offset): (Option<i64>, Option<i64>, Option<i64>) =
            match &heading.source_span {
                Some(span) => (
                    Some(span.line as i64),
                    Some(span.column as i64),
                    Some(span.byte_offset as i64),
                ),
                None => (None, None, None),
            };
        tx.execute(
            "INSERT INTO temp.norn_increment_headings
             (job_id, doc_path, level, text, slug, source_span_line,
              source_span_column, source_span_byte_offset, sequence)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                job_id,
                doc.path.as_str(),
                heading.level as i64,
                heading.text,
                heading.slug,
                line,
                column,
                byte_offset,
                sequence as i64,
            ],
        )?;
    }
    for (sequence, block_id) in doc.block_ids.iter().enumerate() {
        tx.execute(
            "INSERT INTO temp.norn_increment_block_ids
             (job_id, doc_path, block_id, sequence) VALUES (?, ?, ?, ?)",
            params![job_id, doc.path.as_str(), block_id, sequence as i64],
        )?;
    }
    for (sequence, diagnostic) in doc.diagnostics.iter().enumerate() {
        let severity = match diagnostic.severity {
            crate::core::Severity::Warning => "warning",
            crate::core::Severity::Error => "error",
        };
        tx.execute(
            "INSERT INTO temp.norn_increment_diagnostics
             (job_id, doc_path, severity, code, message, detail, sequence)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![
                job_id,
                doc.path.as_str(),
                severity,
                diagnostic.code,
                diagnostic.message,
                diagnostic.detail,
                sequence as i64,
            ],
        )?;
    }
    Ok(())
}

fn stage_file(
    tx: &Transaction,
    job_id: i64,
    file: &VaultFile,
    mtime_ns: i64,
    size_bytes: i64,
) -> Result<(), CacheError> {
    tx.execute(
        "INSERT INTO temp.norn_increment_files
         (job_id, path, ext, size_bytes, mtime_ns) VALUES (?, ?, ?, ?, ?)",
        params![
            job_id,
            file.path.as_str(),
            file.extension.as_deref().unwrap_or(""),
            size_bytes,
            mtime_ns,
        ],
    )?;
    Ok(())
}

fn metadata_for_parsed_bytes(vault_root: &Utf8Path, doc: &Document) -> Option<(i64, i64)> {
    let path = vault_root.join(&doc.path);
    let before_mtime = mtime_ns(&path)?;
    let before_size = size_bytes(&path)?;
    let bytes = std::fs::read(path.as_std_path()).ok()?;
    let after_mtime = mtime_ns(&path)?;
    let after_size = size_bytes(&path)?;
    if (before_mtime, before_size) != (after_mtime, after_size)
        || bytes.len() as i64 != after_size
        || blake3::hash(&bytes).to_hex().as_str() != doc.hash
    {
        return None;
    }
    Some((after_mtime, after_size))
}

fn stable_file_metadata(vault_root: &Utf8Path, path: &Utf8Path) -> Option<(i64, i64)> {
    let absolute = vault_root.join(path);
    let read_regular = || {
        let metadata = std::fs::symlink_metadata(absolute.as_std_path()).ok()?;
        if !metadata.file_type().is_file() {
            return None;
        }
        let mtime = metadata
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos() as i64;
        Some((mtime, metadata.len() as i64))
    };
    let before = read_regular()?;
    let after = read_regular()?;
    (before == after).then_some(after)
}

fn affected_sources_still_match(
    vault_root: &Utf8Path,
    commit: &IncrementCommit,
) -> Result<(), CacheError> {
    for path in &commit.affected_paths {
        if let Some(&file_metadata) = commit.file_metadata.get(path) {
            if stable_file_metadata(vault_root, path) != Some(file_metadata) {
                return Err(CacheError::IncrementSourceDrift { path: path.clone() });
            }
        } else if commit.intentionally_ignored_paths.contains(path) {
            continue;
        } else {
            match std::fs::symlink_metadata(vault_root.join(path).as_std_path()) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Ok(_) | Err(_) => {
                    return Err(CacheError::IncrementSourceDrift { path: path.clone() });
                }
            }
        }

        if let Some(&index) = commit.fresh_docs.get(path) {
            let absolute = vault_root.join(path);
            let matches = std::fs::read(absolute.as_std_path())
                .ok()
                .is_some_and(|bytes| {
                    blake3::hash(&bytes).to_hex().as_str()
                        == commit.fresh_index.documents[index].hash
                });
            if !matches {
                return Err(CacheError::IncrementSourceDrift { path: path.clone() });
            }
        }
    }
    Ok(())
}

fn graph_intentionally_ignores(path: &Utf8Path, patterns: &[String]) -> bool {
    path.components()
        .any(|component| component.as_str().starts_with('.'))
        || crate::graph::is_ignored(path, patterns)
}

fn load_document_hashes(
    conn: &rusqlite::Connection,
) -> Result<std::collections::BTreeMap<Utf8PathBuf, String>, CacheError> {
    let mut statement = conn.prepare("SELECT path, hash FROM documents")?;
    let rows = statement.query_map([], |row| {
        Ok((
            Utf8PathBuf::from(row.get::<_, String>(0)?),
            row.get::<_, String>(1)?,
        ))
    })?;
    let mut hashes = std::collections::BTreeMap::new();
    for row in rows {
        let (path, hash) = row?;
        hashes.insert(path, hash);
    }
    Ok(hashes)
}

fn canonical_extension(extension: Option<&str>) -> Option<&str> {
    extension.filter(|extension| !extension.is_empty())
}

/// Deterministic identity of the graph inputs that can affect global link
/// resolution. Length framing prevents path/hash/extension concatenation
/// ambiguities, and explicit record/option tags keep future extensions safe.
pub(crate) fn graph_fingerprint(index: &GraphIndex) -> String {
    fn field(hasher: &mut blake3::Hasher, value: &str) {
        hasher.update(&(value.len() as u64).to_le_bytes());
        hasher.update(value.as_bytes());
    }

    let mut documents: Vec<_> = index.documents.iter().collect();
    documents.sort_by(|a, b| a.path.cmp(&b.path));
    let mut files: Vec<_> = index.files.iter().collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"norn-cache-graph-fingerprint-v1\0");
    for document in documents {
        hasher.update(b"D");
        field(&mut hasher, document.path.as_str());
        field(&mut hasher, &document.hash);
    }
    for file in files {
        hasher.update(b"F");
        field(&mut hasher, file.path.as_str());
        match canonical_extension(file.extension.as_deref()) {
            Some(extension) => {
                hasher.update(b"1");
                field(&mut hasher, extension);
            }
            None => {
                hasher.update(b"0");
            }
        }
    }
    hasher.finalize().to_hex().to_string()
}

fn stage_link(tx: &Transaction, job_id: i64, sequence: i64, link: &Link) -> Result<(), CacheError> {
    let resolved = link.resolved_path.as_ref().map(|path| path.as_str());
    let source_context = link
        .source_context
        .as_ref()
        .map(|context| link_source_area_str(&context.area));
    let source_context_property = link
        .source_context
        .as_ref()
        .and_then(|context| context.property.as_deref());
    let (span_start, span_end, span_line, span_column): (
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    ) = match &link.source_span {
        Some(span) => (
            Some(span.byte_offset as i64),
            None,
            Some(span.line as i64),
            Some(span.column as i64),
        ),
        None => (None, None, None, None),
    };
    let unresolved_reason = link.unresolved_reason.as_ref().map(unresolved_reason_str);
    let candidates_json = if link.candidates.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&link.candidates).unwrap_or_default())
    };
    let mut statement = tx.prepare_cached(
        "INSERT INTO temp.norn_increment_links
         (job_id, sequence, source_path, raw, kind, target_raw, resolved_path,
          anchor, block_ref, label, source_span_start, source_span_end,
          source_span_line, source_span_column, source_context,
          source_context_property, status, unresolved_reason, candidates_json)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )?;
    statement.execute(params![
        job_id,
        sequence,
        link.source_path.as_str(),
        link.raw,
        link_kind_str(&link.kind),
        link.target,
        resolved,
        link.anchor,
        link.block_ref,
        link.label,
        span_start,
        span_end,
        span_line,
        span_column,
        source_context,
        source_context_property,
        link_status_str(&link.status),
        unresolved_reason,
        candidates_json,
    ])?;
    Ok(())
}

/// Rewrite the entire links table from the authoritative fresh index.
///
/// Link resolution is a GLOBAL function of the whole document set: any doc's
/// path, stem, or alias change can re-resolve links in OTHER, unchanged docs —
/// e.g. adding an alias `foo` to one doc resolves a previously-missing
/// `[[foo]]` in another. A per-doc "does this doc link a changed target"
/// heuristic cannot capture alias-driven (or ambiguity-driven) re-resolution,
/// which made an incremental refresh's link findings diverge from a full
/// rebuild's (NRN-126) — a violation of the same-input-same-output principle.
///
/// `fresh_index` already holds the fully resolved links for every doc (it is a
/// whole-vault parse + resolve), so rewriting the table from it makes an
/// incremental refresh identical to a rebuild by construction. The parse is
/// already paid for above; only the links table is fully rewritten (doc/field/
/// heading rows for unchanged docs are left untouched).
fn rerun_link_resolution(tx: &Transaction, fresh_index: &GraphIndex) -> Result<(), CacheError> {
    tx.execute("DELETE FROM links", [])?;
    for doc in &fresh_index.documents {
        for link in &doc.links {
            insert_link(tx, link)?;
        }
    }
    Ok(())
}

fn clear_all_rows(tx: &rusqlite::Transaction) -> Result<(), CacheError> {
    tx.execute("DELETE FROM documents", [])?;
    tx.execute("DELETE FROM document_fields", [])?;
    tx.execute("DELETE FROM files", [])?;
    tx.execute("DELETE FROM links", [])?;
    tx.execute("DELETE FROM headings", [])?;
    tx.execute("DELETE FROM block_ids", [])?;
    tx.execute("DELETE FROM diagnostics", [])?;
    Ok(())
}

fn insert_document(
    tx: &rusqlite::Transaction,
    vault_root: &Utf8Path,
    doc: &Document,
    report: &mut IndexReport,
    index_set: &std::collections::BTreeSet<String>,
) -> Result<(), CacheError> {
    let absolute = vault_root.join(&doc.path);
    let mtime_ns = mtime_ns(&absolute).unwrap_or(0);
    let size_bytes = size_bytes(&absolute).unwrap_or(0);
    insert_document_with_metadata(tx, doc, report, index_set, mtime_ns, size_bytes)
}

fn insert_document_with_metadata(
    tx: &rusqlite::Transaction,
    doc: &Document,
    report: &mut IndexReport,
    index_set: &std::collections::BTreeSet<String>,
    mtime_ns: i64,
    size_bytes: i64,
) -> Result<(), CacheError> {
    let frontmatter_json = doc
        .frontmatter
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default());

    tx.execute(
        "INSERT INTO documents
           (path, stem, hash, frontmatter_json, body_text, mtime_ns, size_bytes)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        params![
            doc.path.as_str(),
            doc.stem,
            doc.hash,
            frontmatter_json,
            doc.body_text,
            mtime_ns,
            size_bytes,
        ],
    )?;
    report.doc_count += 1;

    crate::cache::document_fields::insert_rows(
        tx,
        doc.path.as_str(),
        doc.frontmatter.as_ref(),
        index_set,
    )?;

    for heading in &doc.headings {
        let (line, column, byte_offset): (Option<i64>, Option<i64>, Option<i64>) =
            match &heading.source_span {
                Some(s) => (
                    Some(s.line as i64),
                    Some(s.column as i64),
                    Some(s.byte_offset as i64),
                ),
                None => (None, None, None),
            };
        tx.execute(
            "INSERT OR IGNORE INTO headings
               (doc_path, level, text, slug,
                source_span_line, source_span_column, source_span_byte_offset)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![
                doc.path.as_str(),
                heading.level as i64,
                heading.text,
                heading.slug,
                line,
                column,
                byte_offset,
            ],
        )?;
    }
    for block_id in &doc.block_ids {
        tx.execute(
            "INSERT OR IGNORE INTO block_ids (doc_path, block_id) VALUES (?, ?)",
            params![doc.path.as_str(), block_id],
        )?;
    }
    for link in &doc.links {
        insert_link(tx, link)?;
        report.link_count += 1;
    }
    for diagnostic in &doc.diagnostics {
        insert_diagnostic(tx, doc.path.as_str(), diagnostic)?;
    }

    Ok(())
}

fn insert_diagnostic(
    tx: &rusqlite::Transaction,
    doc_path: &str,
    diagnostic: &crate::core::Diagnostic,
) -> Result<(), CacheError> {
    let severity = match diagnostic.severity {
        crate::core::Severity::Warning => "warning",
        crate::core::Severity::Error => "error",
    };
    tx.execute(
        "INSERT INTO diagnostics (doc_path, severity, code, message, detail)
         VALUES (?, ?, ?, ?, ?)",
        params![
            doc_path,
            severity,
            diagnostic.code,
            diagnostic.message,
            diagnostic.detail,
        ],
    )?;
    Ok(())
}

fn link_kind_str(kind: &crate::core::LinkKind) -> &'static str {
    match kind {
        crate::core::LinkKind::Wikilink => "wikilink",
        crate::core::LinkKind::Markdown => "markdown",
        crate::core::LinkKind::Embed => "embed",
    }
}

fn link_status_str(status: &crate::core::LinkStatus) -> &'static str {
    match status {
        crate::core::LinkStatus::Resolved => "resolved",
        crate::core::LinkStatus::Unresolved => "unresolved",
        crate::core::LinkStatus::Ambiguous => "ambiguous",
    }
}

fn link_source_area_str(area: &crate::core::LinkSourceArea) -> &'static str {
    match area {
        crate::core::LinkSourceArea::Body => "body",
        crate::core::LinkSourceArea::Frontmatter => "frontmatter",
    }
}

fn unresolved_reason_str(reason: &crate::core::UnresolvedReason) -> &'static str {
    match reason {
        crate::core::UnresolvedReason::TargetMissing => "target-missing",
        crate::core::UnresolvedReason::AnchorMissing => "anchor-missing",
        crate::core::UnresolvedReason::BlockRefMissing => "block-ref-missing",
        crate::core::UnresolvedReason::Ambiguous => "ambiguous",
    }
}

fn insert_link(tx: &rusqlite::Transaction, link: &Link) -> Result<(), CacheError> {
    let kind = link_kind_str(&link.kind);
    let resolved = link.resolved_path.as_ref().map(|p| p.as_str().to_string());
    let status = link_status_str(&link.status);
    let source_context = link
        .source_context
        .as_ref()
        .map(|c| link_source_area_str(&c.area).to_string());
    let source_context_property = link
        .source_context
        .as_ref()
        .and_then(|c| c.property.clone());
    // SourceSpan currently exposes only a single byte offset; store it as
    // span_start and leave span_end NULL until the parser tracks an end.
    // Line/column are persisted in their own columns so the cache round-trip
    // matches `crate::graph::build_index` for downstream consumers that read
    // those fields.
    let (span_start, span_end, span_line, span_column): (
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    ) = match &link.source_span {
        Some(s) => (
            Some(s.byte_offset as i64),
            None,
            Some(s.line as i64),
            Some(s.column as i64),
        ),
        None => (None, None, None, None),
    };
    let unresolved_reason = link.unresolved_reason.as_ref().map(unresolved_reason_str);
    let candidates_json = if link.candidates.is_empty() {
        None
    } else {
        // Serialize candidate paths as a JSON array of strings. Read-side
        // parses with serde_json; failure round-trips as an empty list.
        Some(serde_json::to_string(&link.candidates).unwrap_or_default())
    };
    // `prepare_cached`, not `tx.execute`: link insertion is a hot per-link loop
    // (`rerun_link_resolution` rewrites the WHOLE links table every increment
    // commit), so recompiling this INSERT per row is wasted work. The cached
    // statement is reused across every call on this connection; both callers
    // (`insert_document`'s per-doc loop and the global rewrite) benefit.
    let mut stmt = tx.prepare_cached(
        "INSERT INTO links
           (source_path, raw, kind, target_raw, resolved_path, anchor, block_ref,
            label, source_span_start, source_span_end, source_span_line, source_span_column,
            source_context, source_context_property, status, unresolved_reason, candidates_json)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )?;
    stmt.execute(params![
        link.source_path.as_str(),
        link.raw,
        kind,
        link.target,
        resolved,
        link.anchor,
        link.block_ref,
        link.label,
        span_start,
        span_end,
        span_line,
        span_column,
        source_context,
        source_context_property,
        status,
        unresolved_reason,
        candidates_json,
    ])?;
    Ok(())
}

fn insert_file(
    tx: &rusqlite::Transaction,
    vault_root: &Utf8Path,
    file: &VaultFile,
) -> Result<(), CacheError> {
    let absolute = vault_root.join(&file.path);
    let size = size_bytes(&absolute).unwrap_or(0);
    let mtime = mtime_ns(&absolute).unwrap_or(0);
    insert_file_with_metadata(tx, file, mtime, size)
}

fn insert_file_with_metadata(
    tx: &rusqlite::Transaction,
    file: &VaultFile,
    mtime_ns: i64,
    size_bytes: i64,
) -> Result<(), CacheError> {
    let ext = file.extension.as_deref().unwrap_or("");
    tx.execute(
        "INSERT OR REPLACE INTO files (path, ext, size_bytes, mtime_ns) VALUES (?, ?, ?, ?)",
        params![file.path.as_str(), ext, size_bytes, mtime_ns],
    )?;
    Ok(())
}

fn update_meta_rebuild_ts(tx: &rusqlite::Transaction) -> Result<(), CacheError> {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string();
    tx.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('last_full_rebuild_ts', ?)",
        params![now_secs],
    )?;
    Ok(())
}

/// Stamp the `links_alias_field` meta row with the value the cache was
/// opened with. Always written (empty string when alias support is disabled)
/// so subsequent `Cache::open_with_config` calls can compare against a
/// definite value.
fn update_meta_alias_field(
    tx: &rusqlite::Transaction,
    alias_field: Option<&str>,
) -> Result<(), CacheError> {
    tx.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('links_alias_field', ?)",
        params![alias_field.unwrap_or("")],
    )?;
    Ok(())
}

/// Stamp the `index_set_hash` meta row so a later `Cache::open_with_index`
/// call can tell whether `document_fields` already matches the resolved
/// Wave-2 index set, or needs a re-shred.
fn update_meta_index_set_hash(tx: &rusqlite::Transaction, hash: &str) -> Result<(), CacheError> {
    tx.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('index_set_hash', ?)",
        params![hash],
    )?;
    Ok(())
}

fn update_meta_graph_fingerprint(
    tx: &rusqlite::Transaction,
    fingerprint: &str,
) -> Result<(), CacheError> {
    tx.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('graph_fingerprint', ?)",
        params![fingerprint],
    )?;
    Ok(())
}

fn mtime_ns(path: &Utf8Path) -> Option<i64> {
    std::fs::metadata(path.as_std_path()).ok().and_then(|m| {
        m.modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_nanos() as i64)
    })
}

fn size_bytes(path: &Utf8Path) -> Option<i64> {
    std::fs::metadata(path.as_std_path())
        .ok()
        .map(|m| m.len() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    fn make_vault_with_one_doc() -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        // Create the vault under a non-hidden subdirectory: TempDir's own
        // basename starts with `.tmp`, which vault_graph's WalkDir filter
        // treats as hidden and skips entirely.
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(
            root.join("doc.md").as_std_path(),
            "---\ntitle: Doc\n---\n# Heading\n\nbody [link](other.md)\n",
        )
        .unwrap();
        std::fs::write(
            root.join("other.md").as_std_path(),
            "---\ntitle: Other\n---\n",
        )
        .unwrap();
        (tmp, root)
    }

    /// Vault with an `Archive/` subdir (a *visible* path — the stock ignore
    /// entries are all hidden dirs the scanner skips anyway) plus a live doc
    /// that links `[[archived]]`.
    fn make_vault_with_archive() -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::create_dir(root.join("Archive").as_std_path()).unwrap();
        std::fs::write(
            root.join("Archive/archived.md").as_std_path(),
            "---\ntitle: Archived\n---\n",
        )
        .unwrap();
        std::fs::write(
            root.join("live.md").as_std_path(),
            "---\ntitle: Live\n---\n\nsee [[archived]]\n",
        )
        .unwrap();
        (tmp, root)
    }

    fn doc_count_under(cache: &crate::cache::Cache, prefix: &str) -> i64 {
        cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM documents WHERE path LIKE ?",
                [format!("{prefix}%")],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn baseline_and_reservation(
        cache: &mut crate::cache::Cache,
    ) -> (GraphIndex, IncrementReservation) {
        let baseline = cache.load_graph_index().unwrap();
        let fingerprint = graph_fingerprint(&baseline);
        let reservation = cache.reserve_increment_commit(&fingerprint).unwrap();
        (baseline, reservation)
    }

    #[test]
    fn incremental_refresh_stores_fingerprint_of_persisted_graph() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        std::fs::remove_file(root.join("other.md")).unwrap();
        std::fs::write(root.join("new.md"), "---\n---\n[[doc]]\n").unwrap();
        cache.index_incremental(&root, &Default::default()).unwrap();

        let persisted = cache.load_graph_index().unwrap();
        let stored: String = cache
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'graph_fingerprint'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, graph_fingerprint(&persisted));
        assert!(persisted.files.iter().any(|file| file.path == "new.md"));
        assert!(!persisted.files.iter().any(|file| file.path == "other.md"));
    }

    #[test]
    fn incremental_refresh_publishes_document_recreated_after_deleted_detection() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        std::fs::remove_file(root.join("doc.md")).unwrap();
        let recreated = root.join("doc.md");
        install_after_increment_detect_hook(move || {
            std::fs::write(
                recreated,
                "---\ntitle: Recreated\n---\n# New heading\n\nrecreated body [[other]]\n",
            )
            .unwrap();
        });

        cache.index_incremental(&root, &Default::default()).unwrap();

        let file_count: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path = 'doc.md'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(file_count, 1, "the recreated file must publish");
        let body: String = cache
            .conn
            .query_row(
                "SELECT body_text FROM documents WHERE path = 'doc.md'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(body, "# New heading\n\nrecreated body [[other]]\n");
        let resolved_link_count: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM links
                 WHERE source_path = 'doc.md' AND target_raw = 'other'
                   AND resolved_path = 'other.md'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            resolved_link_count, 1,
            "the recreated document's links must publish coherently"
        );

        let persisted = cache.load_graph_index().unwrap();
        let stored: String = cache
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'graph_fingerprint'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, graph_fingerprint(&persisted));

        let retry = cache.index_incremental(&root, &Default::default()).unwrap();
        assert_eq!(
            retry.doc_count, 0,
            "the coherent publication needs no repair"
        );
        assert_eq!(
            stored,
            graph_fingerprint(&cache.load_graph_index().unwrap())
        );
    }

    #[test]
    fn incremental_refresh_never_blesses_post_parse_bytes_with_parsed_document() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();
        let baseline_fingerprint: String = cache
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'graph_fingerprint'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        std::fs::remove_file(root.join("doc.md")).unwrap();
        let recreated = root.join("doc.md");
        install_after_increment_detect_hook({
            let recreated = recreated.clone();
            move || {
                std::fs::write(&recreated, "---\n---\nPARSED BODY\n").unwrap();
            }
        });
        install_after_increment_parse_hook(move || {
            std::fs::write(&recreated, "---\n---\nLATEST BODY\n").unwrap();
        });

        let error = cache
            .index_incremental(&root, &Default::default())
            .expect_err("post-parse drift must not publish parsed body with later metadata");
        assert!(
            matches!(
                error,
                CacheError::IncrementSourceDrift { ref path }
                    if path == Utf8Path::new("doc.md")
            ),
            "unexpected error: {error:?}"
        );

        let cached_body: String = cache
            .conn
            .query_row(
                "SELECT body_text FROM documents WHERE path = 'doc.md'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            cached_body, "# Heading\n\nbody [link](other.md)\n",
            "a drifted parse must leave the previous coherent snapshot intact"
        );
        let cached_link_count: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM links
                 WHERE source_path = 'doc.md' AND target_raw = 'other.md'
                   AND resolved_path = 'other.md'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            cached_link_count, 1,
            "a drifted parse must not partially publish global links"
        );
        let stored_fingerprint: String = cache
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'graph_fingerprint'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored_fingerprint, baseline_fingerprint);
        assert_eq!(
            stored_fingerprint,
            graph_fingerprint(&cache.load_graph_index().unwrap()),
            "files, documents, links, and fingerprint must remain coherent"
        );

        let retry = cache.index_incremental(&root, &Default::default()).unwrap();
        assert_eq!(retry.doc_count, 1, "the next refresh must heal the drift");
        let healed_body: String = cache
            .conn
            .query_row(
                "SELECT body_text FROM documents WHERE path = 'doc.md'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(healed_body, "LATEST BODY\n");
    }

    #[test]
    fn incremental_refresh_expands_changes_seen_after_detection() {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(root.join("a.md"), "---\n---\nA old\n").unwrap();
        std::fs::write(root.join("b.md"), "---\n---\nB old\n").unwrap();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        std::fs::write(root.join("a.md"), "---\n---\nA changed and larger\n").unwrap();
        let b_path = root.join("b.md");
        let original_mtime = filetime::FileTime::from_last_modification_time(
            &std::fs::metadata(b_path.as_std_path()).unwrap(),
        );
        install_after_increment_detect_hook(move || {
            std::fs::write(&b_path, "---\n---\nB new\n").unwrap();
            filetime::set_file_mtime(b_path.as_std_path(), original_mtime).unwrap();
        });

        cache
            .index_incremental(&root, &ChangeDetectOptions::default())
            .unwrap();

        let bodies: Vec<(String, String)> = cache
            .conn
            .prepare("SELECT path, body_text FROM documents ORDER BY path")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            bodies,
            vec![
                ("a.md".to_string(), "A changed and larger\n".to_string()),
                ("b.md".to_string(), "B new\n".to_string()),
            ],
            "the refresh must expand its affected set to every document observed by the fresh parse"
        );
        let persisted = cache.load_graph_index().unwrap();
        let stored: String = cache
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'graph_fingerprint'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, graph_fingerprint(&persisted));

        let retry = cache
            .index_incremental(&root, &ChangeDetectOptions::default())
            .unwrap();
        assert_eq!(
            retry.doc_count, 0,
            "the coherent publication needs no repair"
        );
        assert_eq!(
            stored,
            graph_fingerprint(&cache.load_graph_index().unwrap())
        );
    }

    #[test]
    fn incremental_refresh_retains_cached_attachment_authority_after_parse_drift() {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(root.join("doc.md"), "---\n---\nold cache body\n").unwrap();
        std::fs::write(root.join("asset.png"), b"png").unwrap();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        std::fs::write(
            root.join("doc.md"),
            "---\n---\nnew body links ![[asset.png]]\n",
        )
        .unwrap();
        let asset = root.join("asset.png");
        install_after_increment_parse_hook(move || {
            std::fs::remove_file(asset).unwrap();
        });

        cache
            .index_incremental(&root, &ChangeDetectOptions::default())
            .unwrap();

        let body: String = cache
            .conn
            .query_row(
                "SELECT body_text FROM documents WHERE path = 'doc.md'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            body, "new body links ![[asset.png]]\n",
            "the affected Markdown row must publish"
        );
        let cached_asset: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path = 'asset.png'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            cached_asset, 1,
            "an unrelated attachment keeps its cached authority"
        );
        let resolved_link: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM links
                 WHERE source_path = 'doc.md' AND target_raw = 'asset.png'
                   AND resolved_path = 'asset.png'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            resolved_link, 1,
            "global links must resolve against the cached attachment overlay"
        );
        let fingerprint: String = cache
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'graph_fingerprint'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            fingerprint,
            graph_fingerprint(&cache.load_graph_index().unwrap()),
            "the publication must fingerprint the exact cached-file overlay"
        );

        let retry = cache
            .index_incremental(&root, &ChangeDetectOptions::default())
            .unwrap();
        assert_eq!(
            retry.doc_count, 0,
            "a clean Markdown retry must not absorb unrelated attachment drift"
        );
        assert_eq!(
            fingerprint,
            graph_fingerprint(&cache.load_graph_index().unwrap()),
            "a clean retry must preserve the coherent cached-file overlay"
        );
    }

    #[test]
    fn trailing_dot_file_fingerprint_survives_cache_round_trip() {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(root.join("doc.md"), "---\n---\nDoc\n").unwrap();
        std::fs::write(root.join("trailing."), b"attachment").unwrap();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        let stored: String = cache
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'graph_fingerprint'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let persisted = cache.load_graph_index().unwrap();
        assert_eq!(
            stored,
            graph_fingerprint(&persisted),
            "empty and absent extensions persist identically and must fingerprint identically"
        );
    }

    #[test]
    fn files_ignore_excludes_docs_at_rebuild() {
        // With Archive/** in files.ignore, a full rebuild must never index the
        // archived doc — not present in the documents table, not queryable,
        // not a link target (NRN-117).
        let (_tmp, root) = make_vault_with_archive();
        let ignore = vec!["Archive/**".to_string()];
        let mut cache = crate::cache::Cache::open_with_index(
            &root,
            None,
            &ignore,
            &std::collections::BTreeSet::new(),
            "test-hash",
        )
        .unwrap();
        cache.rebuild(&root).unwrap();

        assert_eq!(
            doc_count_under(&cache, "Archive/"),
            0,
            "archived docs must be excluded from the cache"
        );
        assert_eq!(doc_count_under(&cache, "live.md"), 1, "live doc stays");

        // The link [[archived]] into the now-ignored target must be unresolved
        // (link-target-missing), not silently resolved to the excluded doc.
        let resolved: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM links WHERE resolved_path LIKE 'Archive/%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(resolved, 0, "no link may resolve into an ignored path");
    }

    #[test]
    fn files_ignore_purges_newly_ignored_on_incremental() {
        // Build WITHOUT ignore (archived doc indexed), then reopen WITH ignore
        // and run an incremental refresh: the newly-ignored doc must be purged,
        // matching a full rebuild (NRN-117 / determinism).
        let (_tmp, root) = make_vault_with_archive();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();
        assert_eq!(doc_count_under(&cache, "Archive/"), 1, "indexed initially");
        drop(cache);

        let ignore = vec!["Archive/**".to_string()];
        let mut cache = crate::cache::Cache::open_with_index(
            &root,
            None,
            &ignore,
            &std::collections::BTreeSet::new(),
            "test-hash",
        )
        .unwrap();
        cache
            .index_incremental(&root, &crate::cache::ChangeDetectOptions::default())
            .unwrap();
        assert_eq!(
            doc_count_under(&cache, "Archive/"),
            0,
            "incremental refresh must purge the newly-ignored doc"
        );
    }

    #[test]
    fn incremental_refresh_matches_rebuild_after_alias_change() {
        // a.md links [[foo]]; b.md initially has no matching alias, so [[foo]] is
        // unresolved. Adding alias `foo` to b.md must make [[foo]] resolve on the
        // NEXT incremental refresh, identically to a full rebuild — link
        // resolution is global, so a change to b's aliases re-resolves a's link
        // even though a itself did not change (NRN-126, the determinism rule).
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(
            root.join("a.md").as_std_path(),
            "---\ntitle: A\n---\n\nsee [[foo]]\n",
        )
        .unwrap();
        std::fs::write(
            root.join("b.md").as_std_path(),
            "---\ntitle: B\n---\n\nB body\n",
        )
        .unwrap();

        let a_resolved = |cache: &crate::cache::Cache| -> Option<String> {
            cache
                .conn
                .query_row(
                    "SELECT resolved_path FROM links WHERE source_path = 'a.md'",
                    [],
                    |r| r.get::<_, Option<String>>(0),
                )
                .unwrap()
        };
        // The determinism invariant is that an incremental refresh leaves the
        // WHOLE links table byte-identical to a rebuild — snapshot every
        // resolution-bearing column in a stable order, not just one row.
        type LinkRow = (String, String, Option<String>, String, Option<String>);
        let links_snapshot = |cache: &crate::cache::Cache| -> Vec<LinkRow> {
            let mut stmt = cache
                .conn
                .prepare(
                    "SELECT source_path, target_raw, resolved_path, status, unresolved_reason \
                     FROM links ORDER BY source_path, target_raw, resolved_path",
                )
                .unwrap();
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ))
                })
                .unwrap();
            rows.map(|r| r.unwrap()).collect()
        };

        // Build with alias_field configured so aliases participate in resolution.
        let mut cache = crate::cache::Cache::open_with_index(
            &root,
            Some("aliases"),
            &[],
            &std::collections::BTreeSet::new(),
            "hash",
        )
        .unwrap();
        cache.rebuild(&root).unwrap();
        assert_eq!(
            a_resolved(&cache),
            None,
            "[[foo]] unresolved before the alias"
        );

        // Add alias `foo` to b.md on disk, then run an incremental refresh.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(
            root.join("b.md").as_std_path(),
            "---\ntitle: B\naliases:\n  - foo\n---\n\nB body\n",
        )
        .unwrap();
        cache
            .index_incremental(&root, &crate::cache::ChangeDetectOptions::default())
            .unwrap();
        let incremental = links_snapshot(&cache);

        // A full rebuild is the determinism oracle.
        cache.rebuild(&root).unwrap();
        let rebuilt = links_snapshot(&cache);

        assert_eq!(
            incremental, rebuilt,
            "the whole links table after an incremental refresh must equal a full rebuild"
        );
        assert_eq!(
            a_resolved(&cache),
            Some("b.md".to_string()),
            "[[foo]] should resolve to b.md via its new alias"
        );
    }

    #[test]
    fn rebuild_populates_documents_table() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        let report = cache.rebuild(&root).unwrap();
        assert_eq!(report.doc_count, 2);

        let count: i64 = cache
            .conn
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn rebuild_populates_links_table() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        let count: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM links WHERE source_path = 'doc.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn rebuild_stores_body_text() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        let body: String = cache
            .conn
            .query_row(
                "SELECT body_text FROM documents WHERE path = 'doc.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(body.contains("# Heading"));
        assert!(body.contains("body [link](other.md)"));
        // Frontmatter not in body_text.
        assert!(!body.contains("title: Doc"));
    }

    #[test]
    fn incremental_picks_up_added_file() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        std::fs::write(
            root.join("third.md").as_std_path(),
            "---\ntitle: Third\n---\n",
        )
        .unwrap();
        let report = cache.index_incremental(&root, &Default::default()).unwrap();
        assert!(report.doc_count >= 1);

        let count: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM documents WHERE path = 'third.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn incremental_removes_deleted_file() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        std::fs::remove_file(root.join("other.md").as_std_path()).unwrap();
        cache.index_incremental(&root, &Default::default()).unwrap();

        let count: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM documents WHERE path = 'other.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);

        // Links targeting other.md should now be unresolved.
        let resolved: Option<String> = cache
            .conn
            .query_row(
                "SELECT resolved_path FROM links WHERE source_path = 'doc.md' AND target_raw = 'other.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(resolved.is_none());
    }

    #[test]
    fn incremental_after_no_changes_is_cheap() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();
        let report = cache.index_incremental(&root, &Default::default()).unwrap();
        assert_eq!(report.doc_count, 0);
        assert_eq!(report.file_count, 0);
    }

    #[test]
    fn incremental_handles_rename_via_delete_plus_add() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        std::fs::rename(
            root.join("other.md").as_std_path(),
            root.join("renamed.md").as_std_path(),
        )
        .unwrap();
        cache.index_incremental(&root, &Default::default()).unwrap();

        let other_count: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM documents WHERE path = 'other.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(other_count, 0);
        let renamed_count: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM documents WHERE path = 'renamed.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(renamed_count, 1);
    }

    #[test]
    fn cache_rebuild_threads_alias_field_through_to_link_resolution() {
        // Regression: prior to this fix, `Cache::rebuild` called
        // `crate::graph::build_index` with default options, which left
        // `alias_field = None` so alias fallback never ran during link
        // resolution. Cached link rows then served alias-blind status to
        // every downstream consumer (validate, find, repair plan, show
        // incoming).
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        // vm.md is the alias target.
        std::fs::write(
            root.join("vm.md").as_std_path(),
            "---\naliases:\n  - Vault Memory\n---\n# Vault Memory\n",
        )
        .unwrap();
        // src.md has a wikilink that only resolves via alias.
        std::fs::write(
            root.join("src.md").as_std_path(),
            "---\n---\n# Source\n\nThis links to [[Vault Memory]].\n",
        )
        .unwrap();

        let mut cache = crate::cache::Cache::open_with_config(&root, Some("aliases")).unwrap();
        cache.rebuild(&root).unwrap();

        let deep = cache
            .document_with_connections(camino::Utf8Path::new("src.md"), false)
            .unwrap()
            .expect("src.md in cache");
        // After rebuild with alias_field set, the [[Vault Memory]] link in
        // src.md must be Resolved (via alias) — NOT Unresolved.
        assert!(
            deep.unresolved_links.is_empty(),
            "expected no unresolved links, got: {:?}",
            deep.unresolved_links
        );
        let alias_link = deep
            .outgoing_links
            .iter()
            .find(|l| l.target == "Vault Memory")
            .expect("[[Vault Memory]] outgoing link");
        assert_eq!(
            alias_link.resolved_path.as_deref(),
            Some(camino::Utf8Path::new("vm.md")),
            "alias link should resolve to vm.md"
        );
    }

    #[test]
    fn rebuild_clears_existing_rows() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();
        // Add a stale row.
        cache
            .conn
            .execute(
                "INSERT INTO documents (path, stem, hash, body_text, mtime_ns, size_bytes) \
                 VALUES ('stale.md', 'stale', 'h', 'b', 0, 0)",
                [],
            )
            .unwrap();
        cache.rebuild(&root).unwrap();
        let count: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM documents WHERE path = 'stale.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    // Concurrency integration test: two simultaneous `rebuild` calls must
    // both complete successfully, with the second one serializing behind
    // the first via the advisory write lock.
    #[test]
    fn two_simultaneous_rebuilds_serialize() {
        let tmp = TempDir::new().unwrap();
        // vault_graph treats hidden directories (basename starts with `.`) as
        // skipped — TempDir's own basename starts with `.tmp`, so nest the
        // vault under a non-hidden subdirectory.
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(
            root.join("a.md").as_std_path(),
            "---\ntitle: A\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            root.join("b.md").as_std_path(),
            "---\ntitle: B\n---\nbody [[a]]\n",
        )
        .unwrap();

        let root1 = root.clone();
        let handle1 = std::thread::spawn(move || {
            let mut cache = crate::cache::Cache::open(&root1).unwrap();
            cache.rebuild(&root1)
        });

        // Tiny stagger so handle1 has reached `rebuild` and acquired the lock
        // before handle2 races for it. Without this the test still asserts both
        // succeed, but with the stagger we exercise the "second writer waits"
        // path deterministically.
        std::thread::sleep(std::time::Duration::from_millis(10));

        let root2 = root.clone();
        let handle2 = std::thread::spawn(move || {
            let mut cache = crate::cache::Cache::open(&root2).unwrap();
            cache.rebuild(&root2)
        });

        let r1 = handle1.join().unwrap();
        let r2 = handle2.join().unwrap();
        assert!(r1.is_ok(), "first rebuild failed: {r1:?}");
        assert!(r2.is_ok(), "second rebuild failed: {r2:?}");
    }

    // ---- Increment-commit primitive (NRN-252 / NRN-158) ---------------------

    type DocRow = (String, String, String); // (path, hash, body_text)
    type LinkRow = (String, String, Option<String>, String); // (src, target, resolved, status)

    fn snapshot_docs(cache: &crate::cache::Cache) -> Vec<DocRow> {
        let mut stmt = cache
            .conn
            .prepare("SELECT path, hash, body_text FROM documents ORDER BY path")
            .unwrap();
        stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
    }

    fn snapshot_links(cache: &crate::cache::Cache) -> Vec<LinkRow> {
        let mut stmt = cache
            .conn
            .prepare(
                "SELECT source_path, target_raw, resolved_path, status \
                 FROM links ORDER BY source_path, target_raw, resolved_path",
            )
            .unwrap();
        stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
            ))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
    }

    type OwnedSnapshot = Vec<(&'static str, Vec<Vec<rusqlite::types::Value>>)>;

    fn snapshot_owned_rows(conn: &rusqlite::Connection) -> OwnedSnapshot {
        [
            (
                "files",
                "SELECT path, ext, size_bytes, mtime_ns FROM files ORDER BY path",
            ),
            (
                "documents",
                "SELECT path, stem, hash, frontmatter_json, body_text, mtime_ns, size_bytes \
                 FROM documents ORDER BY path",
            ),
            (
                "document_fields",
                "SELECT path, key, value FROM document_fields \
                 ORDER BY path, key, typeof(value), value",
            ),
            (
                "headings",
                "SELECT doc_path, level, text, slug, source_span_line, source_span_column, \
                        source_span_byte_offset FROM headings ORDER BY doc_path, slug",
            ),
            (
                "block_ids",
                "SELECT doc_path, block_id FROM block_ids ORDER BY doc_path, block_id",
            ),
            (
                "diagnostics",
                "SELECT doc_path, severity, code, message, detail FROM diagnostics \
                 ORDER BY doc_path, severity, code, message, detail",
            ),
            (
                "links",
                "SELECT source_path, raw, kind, target_raw, resolved_path, anchor, block_ref, \
                        label, source_span_start, source_span_end, source_span_line, \
                        source_span_column, source_context, source_context_property, status, \
                        unresolved_reason, candidates_json FROM links \
                 ORDER BY source_path, raw, target_raw, resolved_path",
            ),
        ]
        .into_iter()
        .map(|(table, sql)| {
            let mut stmt = conn.prepare(sql).unwrap();
            let column_count = stmt.column_count();
            let rows = stmt
                .query_map([], |row| {
                    (0..column_count)
                        .map(|column| row.get::<_, rusqlite::types::Value>(column))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .map(|row| row.unwrap())
                .collect();
            (table, rows)
        })
        .collect()
    }

    #[test]
    fn increment_chunk_budget_defaults_to_fifty_ms() {
        // Other increment tests deliberately set the process-global debug
        // override while running in parallel, so calling the env-sensitive
        // accessor here is inherently racy. The boundary tests exercise that
        // override; this test owns the production default contract itself.
        assert_eq!(INCREMENT_CHUNK_BUDGET, Duration::from_millis(50));
    }

    /// The full NRN-158 correctness contract: an increment commit fed the three
    /// change kinds (modified, created, deleted) leaves the cache byte-identical
    /// to a full rebuild of the same disk state, AND a subsequent `detect`
    /// reports zero changes (so the next refresh does no whole-vault rebuild). A
    /// tiny budget forces the file-coherent, multi-chunk path.
    #[test]
    fn increment_commit_matches_rebuild_for_all_change_kinds() {
        let (_tmp, root) = make_vault_with_one_doc(); // doc.md -> other.md, other.md
        let index_set = ["title".to_string()].into_iter().collect();
        let mut cache = crate::cache::Cache::open_with_index(
            &root,
            None,
            &[],
            &index_set,
            "atomic-snapshot-proof",
        )
        .unwrap();
        cache.rebuild(&root).unwrap();
        let reader = rusqlite::Connection::open(cache.cache_dir.join("cache.db")).unwrap();
        let old_docs = snapshot_docs(&cache);
        let old_links = snapshot_links(&cache);
        let old_owned_rows = snapshot_owned_rows(&reader);

        // Modified (doc.md, now links [[new]]), created (new.md), deleted (other.md).
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(
            root.join("doc.md").as_std_path(),
            "---\ntitle: Doc2\n---\n# H2\n\nbody [[new]]\n",
        )
        .unwrap();
        std::fs::write(root.join("new.md").as_std_path(), "---\ntitle: New\n---\n").unwrap();
        std::fs::remove_file(root.join("other.md").as_std_path()).unwrap();

        let changed: Vec<Utf8PathBuf> = vec!["doc.md".into(), "new.md".into(), "other.md".into()];
        let (baseline, reservation) = baseline_and_reservation(&mut cache);
        let mut commit = crate::cache::Cache::begin_increment_commit(
            &root,
            &changed,
            cache.alias_field.as_deref(),
            &cache.files_ignore,
            &reservation,
            baseline,
        )
        .unwrap();
        let mut chunks = 0usize;
        loop {
            let more = cache
                .commit_increment_chunk(&root, &mut commit, Duration::from_millis(0))
                .unwrap();
            chunks += 1;
            if !more {
                break;
            }
            let docs: Vec<DocRow> = {
                let mut stmt = reader
                    .prepare("SELECT path, hash, body_text FROM documents ORDER BY path")
                    .unwrap();
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                    .unwrap()
                    .map(|r| r.unwrap())
                    .collect()
            };
            let links: Vec<(String, String, Option<String>, String)> = {
                let mut stmt = reader
                    .prepare(
                        "SELECT source_path, target_raw, resolved_path, status \
                         FROM links ORDER BY source_path, target_raw, resolved_path",
                    )
                    .unwrap();
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                    .unwrap()
                    .map(|r| r.unwrap())
                    .collect()
            };
            assert_eq!(
                docs, old_docs,
                "a separate reader must see the entire old document snapshot while staging"
            );
            assert_eq!(
                links, old_links,
                "a separate reader must see the entire old links snapshot while staging"
            );
            assert_eq!(
                snapshot_owned_rows(&reader),
                old_owned_rows,
                "a separate reader must see every old document-owned row while staging"
            );
        }
        assert!(
            chunks >= 4,
            "a 0ms budget must force one-file-per-chunk plus the link phase (>=4), got {chunks}"
        );

        // The point of NRN-158: a subsequent detect finds NOTHING, so the next
        // refresh skips the whole-vault rebuild.
        let after = detect(&root, &cache, &ChangeDetectOptions::default()).unwrap();
        assert!(
            after.is_empty(),
            "increment commit must leave detect empty, got {after:?}"
        );

        let inc_docs = snapshot_docs(&cache);
        let inc_links = snapshot_links(&cache);
        let inc_owned_rows = snapshot_owned_rows(&cache.conn);

        // Oracle: a full rebuild of the identical disk state.
        cache.rebuild(&root).unwrap();
        assert_eq!(
            inc_docs,
            snapshot_docs(&cache),
            "increment-committed document rows must equal a full rebuild"
        );
        assert_eq!(
            inc_links,
            snapshot_links(&cache),
            "increment-committed link rows must equal a full rebuild (global resolution)"
        );
        assert_eq!(
            inc_owned_rows,
            snapshot_owned_rows(&cache.conn),
            "every increment-published document-owned row must equal a full rebuild"
        );
    }

    /// Increment commit re-resolves a link in an UNCHANGED file when a changed
    /// file's alias newly matches it — the global links rewrite, proving the
    /// commit is not merely a per-file row swap.
    #[test]
    fn increment_commit_reresolves_links_in_unchanged_files() {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        // a.md (never in the changed set) links [[foo]], initially unresolved.
        std::fs::write(
            root.join("a.md").as_std_path(),
            "---\ntitle: A\n---\n\nsee [[foo]]\n",
        )
        .unwrap();
        std::fs::write(
            root.join("b.md").as_std_path(),
            "---\ntitle: B\n---\n\nbody\n",
        )
        .unwrap();
        let mut cache = crate::cache::Cache::open_with_index(
            &root,
            Some("aliases"),
            &[],
            &std::collections::BTreeSet::new(),
            "hash",
        )
        .unwrap();
        cache.rebuild(&root).unwrap();

        // Add alias `foo` to b.md — only b.md is in the changed set.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(
            root.join("b.md").as_std_path(),
            "---\ntitle: B\naliases:\n  - foo\n---\n\nbody\n",
        )
        .unwrap();

        let changed: Vec<Utf8PathBuf> = vec!["b.md".into()];
        let (baseline, reservation) = baseline_and_reservation(&mut cache);
        let mut commit = crate::cache::Cache::begin_increment_commit(
            &root,
            &changed,
            cache.alias_field.as_deref(),
            &cache.files_ignore,
            &reservation,
            baseline,
        )
        .unwrap();
        while cache
            .commit_increment_chunk(&root, &mut commit, increment_chunk_budget())
            .unwrap()
        {}

        let a_resolved: Option<String> = cache
            .conn
            .query_row(
                "SELECT resolved_path FROM links WHERE source_path = 'a.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            a_resolved.as_deref(),
            Some("b.md"),
            "[[foo]] in the UNCHANGED a.md must re-resolve via b.md's new alias"
        );
    }

    /// An external writer that publishes after TEMP staging starts wins. The
    /// old staged parse observes a changed `data_version` under the terminal
    /// WriteLock + IMMEDIATE transaction and retires without touching main.
    #[test]
    fn increment_does_not_overwrite_an_intervening_external_publication() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut staged_writer = crate::cache::Cache::open(&root).unwrap();
        staged_writer.rebuild(&root).unwrap();

        let (baseline, reservation) = baseline_and_reservation(&mut staged_writer);
        std::fs::write(
            root.join("doc.md"),
            "---\ntitle: Staged\n---\n# Staged\n\nold staged parse\n",
        )
        .unwrap();
        let mut commit = crate::cache::Cache::begin_increment_commit(
            &root,
            &["doc.md".into()],
            staged_writer.alias_field.as_deref(),
            &staged_writer.files_ignore,
            &reservation,
            baseline,
        )
        .unwrap();
        assert!(
            staged_writer
                .commit_increment_chunk(&root, &mut commit, Duration::ZERO)
                .unwrap(),
            "the first call stages the old parse"
        );

        std::fs::write(
            root.join("doc.md"),
            "---\ntitle: Newer\n---\n# Newer\n\nexternal publication wins\n",
        )
        .unwrap();
        let mut external_writer = crate::cache::Cache::open(&root).unwrap();
        external_writer.rebuild(&root).unwrap();
        drop(external_writer);

        while staged_writer
            .commit_increment_chunk(&root, &mut commit, Duration::ZERO)
            .unwrap()
        {}

        let body: String = staged_writer
            .conn
            .query_row(
                "SELECT body_text FROM documents WHERE path = 'doc.md'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            body.contains("external publication wins"),
            "the stale staged parse must not overwrite a newer external publication: {body}"
        );
    }

    /// An external publication may land after reservation validates the
    /// caller's graph fingerprint but before it records a TEMP job. The
    /// reservation's data-version baseline must predate that window so the
    /// terminal publication retires the stale parse.
    #[test]
    fn increment_does_not_publish_after_fingerprint_check_race() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut staged_writer = crate::cache::Cache::open(&root).unwrap();
        staged_writer.rebuild(&root).unwrap();

        let baseline = staged_writer.load_graph_index().unwrap();
        let fingerprint = graph_fingerprint(&baseline);
        let hook_root = root.clone();
        install_after_increment_fingerprint_check_hook(move || {
            std::fs::write(
                hook_root.join("doc.md"),
                "---\ntitle: External\n---\n# External\n\nexternal publication wins\n",
            )
            .unwrap();
            let mut external_writer = crate::cache::Cache::open(&hook_root).unwrap();
            external_writer.rebuild(&hook_root).unwrap();
            drop(external_writer);

            std::fs::write(
                hook_root.join("doc.md"),
                "---\ntitle: Staged\n---\n# Staged\n\nstale staged parse\n",
            )
            .unwrap();
        });
        let reservation = staged_writer
            .reserve_increment_commit(&fingerprint)
            .unwrap();
        let mut commit = crate::cache::Cache::begin_increment_commit(
            &root,
            &["doc.md".into()],
            staged_writer.alias_field.as_deref(),
            &staged_writer.files_ignore,
            &reservation,
            baseline,
        )
        .unwrap();

        while staged_writer
            .commit_increment_chunk(&root, &mut commit, Duration::ZERO)
            .unwrap()
        {}

        let body: String = staged_writer
            .conn
            .query_row(
                "SELECT body_text FROM documents WHERE path = 'doc.md'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            body.contains("external publication wins"),
            "the stale parse must not publish across the fingerprint/data-version window: {body}"
        );
    }

    /// TEMP staging may preserve duplicate parsed identities by sequence, but
    /// terminal publication must retain rebuild's deterministic first-occurrence
    /// `INSERT OR IGNORE` behavior for heading slugs and block IDs.
    #[test]
    fn increment_duplicate_headings_and_block_ids_match_rebuild() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();
        let (baseline, reservation) = baseline_and_reservation(&mut cache);
        std::fs::write(
            root.join("doc.md"),
            "---\ntitle: Duplicates\n---\n# Same\n# Same\nfirst ^dup\nsecond ^dup\n",
        )
        .unwrap();
        let mut commit = crate::cache::Cache::begin_increment_commit(
            &root,
            &["doc.md".into()],
            cache.alias_field.as_deref(),
            &cache.files_ignore,
            &reservation,
            baseline,
        )
        .unwrap();
        while cache
            .commit_increment_chunk(&root, &mut commit, Duration::ZERO)
            .unwrap()
        {}
        let increment_rows = snapshot_owned_rows(&cache.conn);
        let heading_count: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM headings WHERE doc_path = 'doc.md' AND slug = 'same'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let block_count: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM block_ids WHERE doc_path = 'doc.md' AND block_id = 'dup'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            heading_count, 1,
            "duplicate slug must collapse like rebuild"
        );
        assert_eq!(
            block_count, 1,
            "duplicate block ID must collapse like rebuild"
        );

        cache.rebuild(&root).unwrap();
        assert_eq!(
            increment_rows,
            snapshot_owned_rows(&cache.conn),
            "duplicate heading/block publication must be rebuild-identical"
        );
    }

    /// Two jobs may reserve before either enters bulk. Whichever publishes first
    /// revokes the other's same-connection authority, so queue reorder cannot let
    /// an older parse overwrite the winner.
    #[test]
    fn increment_publication_supersedes_an_older_reserved_job() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        let baseline = cache.load_graph_index().unwrap();
        let fingerprint = graph_fingerprint(&baseline);
        let older_reservation = cache.reserve_increment_commit(&fingerprint).unwrap();
        std::fs::write(root.join("doc.md"), "---\n---\nOLDER RESERVED\n").unwrap();
        let mut older = crate::cache::Cache::begin_increment_commit(
            &root,
            &["doc.md".into()],
            cache.alias_field.as_deref(),
            &cache.files_ignore,
            &older_reservation,
            baseline.clone(),
        )
        .unwrap();

        let newer_reservation = cache.reserve_increment_commit(&fingerprint).unwrap();
        std::fs::write(root.join("doc.md"), "---\n---\nNEWER WINNER\n").unwrap();
        let mut newer = crate::cache::Cache::begin_increment_commit(
            &root,
            &["doc.md".into()],
            cache.alias_field.as_deref(),
            &cache.files_ignore,
            &newer_reservation,
            baseline,
        )
        .unwrap();

        while cache
            .commit_increment_chunk(&root, &mut newer, Duration::ZERO)
            .unwrap()
        {}
        while cache
            .commit_increment_chunk(&root, &mut older, Duration::ZERO)
            .unwrap()
        {}
        let body: String = cache
            .conn
            .query_row(
                "SELECT body_text FROM documents WHERE path = 'doc.md'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(body.contains("NEWER WINNER"));
    }

    /// Refresh revocation is non-fallible and precedes TEMP cleanup. Even an
    /// injected DELETE failure cannot leave an old marker with publication
    /// authority after the refresh has committed main.
    #[test]
    fn refresh_cleanup_failure_still_revokes_reserved_increment() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();
        let (baseline, reservation) = baseline_and_reservation(&mut cache);
        std::fs::write(root.join("doc.md"), "---\n---\nOLDER PARSE\n").unwrap();
        let mut commit = crate::cache::Cache::begin_increment_commit(
            &root,
            &["doc.md".into()],
            cache.alias_field.as_deref(),
            &cache.files_ignore,
            &reservation,
            baseline,
        )
        .unwrap();

        std::fs::write(root.join("doc.md"), "---\n---\nREFRESH WINS\n").unwrap();
        cache.index_incremental(&root, &Default::default()).unwrap();
        cache
            .conn
            .execute_batch(
                "CREATE TEMP TRIGGER fail_increment_job_delete
                 BEFORE DELETE ON norn_increment_jobs
                 BEGIN SELECT RAISE(FAIL, 'injected cleanup failure'); END;",
            )
            .unwrap();
        cache.supersede_staged_increments_after_refresh();

        assert!(!cache
            .commit_increment_chunk(&root, &mut commit, Duration::ZERO)
            .unwrap());
        let body: String = cache
            .conn
            .query_row(
                "SELECT body_text FROM documents WHERE path = 'doc.md'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(body.contains("REFRESH WINS"));
    }

    #[test]
    fn published_increment_remains_success_when_temp_cleanup_fails() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();
        let (baseline, reservation) = baseline_and_reservation(&mut cache);
        std::fs::write(root.join("doc.md"), "---\n---\nPUBLISHED\n").unwrap();
        let mut commit = crate::cache::Cache::begin_increment_commit(
            &root,
            &["doc.md".into()],
            cache.alias_field.as_deref(),
            &cache.files_ignore,
            &reservation,
            baseline,
        )
        .unwrap();
        while !matches!(commit.phase, IncrementPhase::Ready) {
            assert!(cache
                .commit_increment_chunk(&root, &mut commit, Duration::ZERO)
                .unwrap());
        }
        cache
            .conn
            .execute_batch(
                "CREATE TEMP TRIGGER fail_published_cleanup
                 BEFORE DELETE ON norn_increment_jobs
                 BEGIN SELECT RAISE(FAIL, 'cleanup failed'); END;",
            )
            .unwrap();
        assert!(!cache
            .commit_increment_chunk(&root, &mut commit, Duration::ZERO)
            .expect("post-publication cleanup is best-effort"));
    }

    #[test]
    fn increment_never_blesses_old_parsed_bytes_with_newer_metadata() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();
        let (baseline, reservation) = baseline_and_reservation(&mut cache);
        let parsed = "---\n---\nOLD PARSED\n";
        let rewritten = "---\n---\nNEW BYTES!\n";
        assert_eq!(parsed.len(), rewritten.len());
        std::fs::write(root.join("doc.md"), parsed).unwrap();
        let mut commit = crate::cache::Cache::begin_increment_commit(
            &root,
            &["doc.md".into()],
            cache.alias_field.as_deref(),
            &cache.files_ignore,
            &reservation,
            baseline,
        )
        .unwrap();
        std::fs::write(root.join("doc.md"), rewritten).unwrap();
        let error = loop {
            match cache.commit_increment_chunk(&root, &mut commit, Duration::ZERO) {
                Ok(true) => {}
                Ok(false) => panic!("affected source drift must not complete successfully"),
                Err(error) => break error,
            }
        };
        assert!(matches!(error, CacheError::IncrementSourceDrift { .. }));
        assert_eq!(cache.staged_increment_job_count(), 0);
        assert!(
            cache.detect_change_count(&root) > 0,
            "a rewrite after parse must remain detectable rather than blessed fresh"
        );
    }

    #[test]
    fn unrelated_source_drift_stays_outside_overlay_while_affected_change_publishes() {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(root.join("a.md"), "---\n---\nA old\n").unwrap();
        std::fs::write(root.join("b.md"), "---\n---\n[[old-target]]\n").unwrap();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();
        let (baseline, reservation) = baseline_and_reservation(&mut cache);
        std::fs::write(root.join("a.md"), "---\n---\nA changed\n").unwrap();
        std::fs::write(root.join("b.md"), "---\n---\n[[new-target]]\n").unwrap();
        let mut commit = crate::cache::Cache::begin_increment_commit(
            &root,
            &["a.md".into()],
            cache.alias_field.as_deref(),
            &cache.files_ignore,
            &reservation,
            baseline,
        )
        .unwrap();
        while cache
            .commit_increment_chunk(&root, &mut commit, Duration::ZERO)
            .unwrap()
        {}
        let new_links: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM links WHERE source_path = 'b.md' AND target_raw = 'new-target'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            new_links, 0,
            "unrelated B disk drift must not enter overlay links"
        );
        let old_links: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM links WHERE source_path = 'b.md' AND target_raw = 'old-target'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_links, 1, "overlay keeps cached B links");
        let a_body: String = cache
            .conn
            .query_row(
                "SELECT body_text FROM documents WHERE path = 'a.md'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            a_body.contains("A changed"),
            "affected A must still publish despite unrelated B drift"
        );
    }

    /// The caller captures its pre-apply graph before reserving publication.
    /// If a refresh publishes an unrelated document in that interval, the old
    /// graph no longer belongs to the reservation's main snapshot and must not
    /// replace the refreshed global links.
    #[test]
    fn increment_does_not_publish_baseline_older_than_its_reservation() {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(root.join("a.md"), "---\n---\nA old\n").unwrap();
        std::fs::write(root.join("b.md"), "---\n---\n[[old-target]]\n").unwrap();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        let old_baseline = cache.load_graph_index().unwrap();
        let old_fingerprint = graph_fingerprint(&old_baseline);
        std::fs::write(root.join("b.md"), "---\n---\n[[new-target]]\n").unwrap();
        cache.index_incremental(&root, &Default::default()).unwrap();

        let error = cache
            .reserve_increment_commit(&old_fingerprint)
            .expect_err("an older baseline must be rejected before reserving");
        assert!(matches!(error, CacheError::IncrementBaselineDrift));
        assert_eq!(
            cache.staged_increment_job_count(),
            0,
            "baseline refusal must not leak a TEMP reservation"
        );

        let new_links: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM links WHERE source_path = 'b.md' AND target_raw = 'new-target'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let old_links: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM links WHERE source_path = 'b.md' AND target_raw = 'old-target'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(new_links, 1, "the reservation's refreshed links must win");
        assert_eq!(old_links, 0, "an older baseline must not publish");
    }

    #[test]
    fn increment_updates_affected_files_with_documents_and_links() {
        let (_tmp, root) = make_vault_with_one_doc();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();
        let (baseline, reservation) = baseline_and_reservation(&mut cache);
        std::fs::remove_file(root.join("other.md")).unwrap();
        std::fs::write(root.join("new.md"), "---\n---\n[[asset.png]]\n").unwrap();
        std::fs::write(root.join("asset.png"), b"png").unwrap();
        let mut commit = crate::cache::Cache::begin_increment_commit(
            &root,
            &["other.md".into(), "new.md".into(), "asset.png".into()],
            cache.alias_field.as_deref(),
            &cache.files_ignore,
            &reservation,
            baseline,
        )
        .unwrap();
        while cache
            .commit_increment_chunk(&root, &mut commit, Duration::ZERO)
            .unwrap()
        {}
        for path in ["new.md", "asset.png"] {
            let count: i64 = cache
                .conn
                .query_row("SELECT COUNT(*) FROM files WHERE path = ?", [path], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 1, "affected file {path} must publish");
        }
        let deleted: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path = 'other.md'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(deleted, 0);
        let persisted = cache.load_graph_index().unwrap();
        let stored: String = cache
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'graph_fingerprint'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stored,
            graph_fingerprint(&persisted),
            "increment publication must stamp the graph it committed"
        );
    }

    #[test]
    fn increment_degrades_when_staged_attachment_disappears_before_publication() {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(root.join("doc.md"), "---\n---\n[[asset.png]]\n").unwrap();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        let baseline = cache.load_graph_index().unwrap();
        let fingerprint = graph_fingerprint(&baseline);
        std::fs::write(root.join("asset.png"), b"png").unwrap();
        let reservation = cache.reserve_increment_commit(&fingerprint).unwrap();
        let mut commit = crate::cache::Cache::begin_increment_commit(
            &root,
            &["asset.png".into()],
            cache.alias_field.as_deref(),
            &cache.files_ignore,
            &reservation,
            baseline,
        )
        .unwrap();
        assert!(cache
            .commit_increment_chunk(&root, &mut commit, Duration::ZERO)
            .unwrap());
        std::fs::remove_file(root.join("asset.png")).unwrap();

        let error = loop {
            match cache.commit_increment_chunk(&root, &mut commit, Duration::ZERO) {
                Ok(true) => {}
                Ok(false) => panic!("attachment drift must degrade, not publish"),
                Err(error) => break error,
            }
        };
        assert!(matches!(
            error,
            CacheError::IncrementSourceDrift { ref path } if path == Utf8Path::new("asset.png")
        ));
        let cached: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path = 'asset.png'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cached, 0, "the vanished attachment must not publish");
    }

    #[cfg(unix)]
    #[test]
    fn increment_degrades_when_staged_attachment_becomes_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(root.join("doc.md"), "---\n---\n[[asset.png]]\n").unwrap();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        let baseline = cache.load_graph_index().unwrap();
        let fingerprint = graph_fingerprint(&baseline);
        let asset = root.join("asset.png");
        std::fs::write(&asset, b"png").unwrap();
        let asset_mtime = filetime::FileTime::from_last_modification_time(
            &std::fs::metadata(asset.as_std_path()).unwrap(),
        );
        let reservation = cache.reserve_increment_commit(&fingerprint).unwrap();
        let mut commit = crate::cache::Cache::begin_increment_commit(
            &root,
            &["asset.png".into()],
            cache.alias_field.as_deref(),
            &cache.files_ignore,
            &reservation,
            baseline,
        )
        .unwrap();
        assert!(cache
            .commit_increment_chunk(&root, &mut commit, Duration::ZERO)
            .unwrap());

        let target = Utf8PathBuf::from_path_buf(tmp.path().join("target.png")).unwrap();
        std::fs::write(&target, b"png").unwrap();
        filetime::set_file_mtime(target.as_std_path(), asset_mtime).unwrap();
        std::fs::remove_file(&asset).unwrap();
        symlink(target.as_std_path(), asset.as_std_path()).unwrap();

        let error = loop {
            match cache.commit_increment_chunk(&root, &mut commit, Duration::ZERO) {
                Ok(true) => {}
                Ok(false) => panic!("symlink substitution must degrade, not publish"),
                Err(error) => break error,
            }
        };
        assert!(matches!(
            error,
            CacheError::IncrementSourceDrift { ref path } if path == Utf8Path::new("asset.png")
        ));
        let cached: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path = 'asset.png'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cached, 0, "the substituted symlink must not publish");
    }

    #[test]
    fn increment_degrades_when_deleted_attachment_is_recreated_before_publication() {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(root.join("doc.md"), "---\n---\n[[asset.png]]\n").unwrap();
        std::fs::write(root.join("asset.png"), b"old").unwrap();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        let baseline = cache.load_graph_index().unwrap();
        let fingerprint = graph_fingerprint(&baseline);
        std::fs::remove_file(root.join("asset.png")).unwrap();
        let reservation = cache.reserve_increment_commit(&fingerprint).unwrap();
        let mut commit = crate::cache::Cache::begin_increment_commit(
            &root,
            &["asset.png".into()],
            cache.alias_field.as_deref(),
            &cache.files_ignore,
            &reservation,
            baseline,
        )
        .unwrap();
        assert!(cache
            .commit_increment_chunk(&root, &mut commit, Duration::ZERO)
            .unwrap());
        std::fs::write(root.join("asset.png"), b"recreated").unwrap();

        let error = loop {
            match cache.commit_increment_chunk(&root, &mut commit, Duration::ZERO) {
                Ok(true) => {}
                Ok(false) => panic!("recreated source must degrade, not publish"),
                Err(error) => break error,
            }
        };
        assert!(matches!(
            error,
            CacheError::IncrementSourceDrift { ref path } if path == Utf8Path::new("asset.png")
        ));
        let cached: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path = 'asset.png'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cached, 1, "the prior cache row must survive degradation");
    }

    #[test]
    fn move_into_ignored_destination_publishes_source_removal() {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir_all(root.join("Archive")).unwrap();
        std::fs::write(root.join("a.md"), "---\n---\nA\n").unwrap();
        let ignore = vec!["Archive/**".to_string()];
        let mut cache = crate::cache::Cache::open_with_index(
            &root,
            None,
            &ignore,
            &std::collections::BTreeSet::new(),
            "ignore-proof",
        )
        .unwrap();
        cache.rebuild(&root).unwrap();
        let (baseline, reservation) = baseline_and_reservation(&mut cache);
        std::fs::rename(root.join("a.md"), root.join("Archive/a.md")).unwrap();
        let mut commit = crate::cache::Cache::begin_increment_commit(
            &root,
            &["a.md".into(), "Archive/a.md".into()],
            cache.alias_field.as_deref(),
            &cache.files_ignore,
            &reservation,
            baseline,
        )
        .unwrap();
        while cache
            .commit_increment_chunk(&root, &mut commit, Duration::ZERO)
            .unwrap()
        {}
        let remaining: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM documents WHERE path = 'a.md'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0, "ignored destination move removes source");
    }

    #[test]
    fn move_into_hidden_destination_publishes_source_removal() {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir_all(root.join(".Archive")).unwrap();
        std::fs::write(root.join("a.md"), "---\n---\nA\n").unwrap();
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        let baseline = cache.load_graph_index().unwrap();
        let fingerprint = graph_fingerprint(&baseline);
        std::fs::rename(root.join("a.md"), root.join(".Archive/a.md")).unwrap();
        let reservation = cache.reserve_increment_commit(&fingerprint).unwrap();
        let mut commit = crate::cache::Cache::begin_increment_commit(
            &root,
            &["a.md".into(), ".Archive/a.md".into()],
            cache.alias_field.as_deref(),
            &cache.files_ignore,
            &reservation,
            baseline,
        )
        .unwrap();
        while cache
            .commit_increment_chunk(&root, &mut commit, Duration::ZERO)
            .unwrap()
        {}

        let remaining: i64 = cache
            .conn
            .query_row(
                "SELECT COUNT(*) FROM documents WHERE path IN ('a.md', '.Archive/a.md')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0, "hidden destination stays outside the graph");
    }
}
