//! Cache writer: full build, the request-boundary incremental refresh, and the
//! chunked mutation-increment pipeline.
//!
//! # Full build (ADR 0003)
//!
//! [`full_build`](Cache::full_build) is the donor `Cache::rebuild` shape: walk
//! the vault via [`crate::graph::build_index_with_options`], clear every table,
//! reinsert every document/file/link/heading/block-id/diagnostic and the EAV
//! rows, then `ANALYZE document_fields` so the planner has real cardinalities.
//! It takes no write lock — the engine has a single writer thread by
//! construction (ADR 0017).
//!
//! # Incremental refresh
//!
//! [`index_incremental`](Cache::index_incremental) is the freshness-refresh path
//! the coalesced refresh ticket drives: detect changes, re-parse the whole vault
//! for authority, drop+reinsert the affected documents, then re-resolve the
//! WHOLE links table (ADR — whole-table link re-resolution per increment: link
//! resolution is a global function of the document set, so this is what keeps an
//! incremental refresh identical to a full build).
//!
//! # Publication-time drift re-proof (the amended trust boundary)
//!
//! The request-boundary [`FreshnessProbe`](crate::cache::freshness::FreshnessProbe)
//! authorizes *serving* a generation; it is NOT the authority for
//! publication-time drift. A file written between the parse and the commit would
//! otherwise land stale content under a fresh `(mtime,size)` baseline that the
//! stat-sweep probe — which compares stats and never re-hashes — reads as Fresh
//! forever. So every commit site ([`index_incremental`](Cache::index_incremental)
//! and the chunked increment's [`begin`](Cache::begin_increment_commit) +
//! terminal publish) re-reads each affected file, requires the re-read hash to
//! equal the parsed document's expected hash, and takes the committed
//! `(mtime,size)` from that same stable observation — aborting with
//! [`CacheError::IncrementSourceDrift`] on mismatch. The donor's broader
//! `PublicationAuthority` (channel/symlink/excluded-subtree re-proof) is gone;
//! the load-bearing parsed-hash re-proof stays.
//!
//! # Chunked mutation increments (ADR 0013 / 0014, DORMANT here)
//!
//! The staging → terminal-publish pipeline lands whole (schema incl. staging,
//! epoch supersession, the `data_version` guard) but is exercised only by
//! engine-level tests in phase 2; the owner wires apply-time mutation to it in
//! phase 3.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use rusqlite::{params, Transaction, TransactionBehavior};

use crate::cache::change_detection::{detect, ChangeDetectOptions, FileChange};
use crate::cache::error::CacheError;
use crate::domain::{Document, GraphIndex, Link, VaultFile};

#[derive(Debug, Clone, Default)]
pub struct IndexReport {
    pub doc_count: usize,
    pub link_count: usize,
    pub file_count: usize,
    pub duration_ms: u128,
}

#[cfg(test)]
std::thread_local! {
    /// Fires once inside [`Cache::index_incremental`] AFTER the whole-vault parse
    /// and BEFORE the commit transaction's drift re-proof, so a test can race a
    /// filesystem write into that window and prove the re-proof aborts.
    static AFTER_INCREMENT_PARSE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
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
pub(crate) fn install_after_increment_parse_hook(hook: impl FnOnce() + 'static) {
    AFTER_INCREMENT_PARSE.with(|slot| {
        let previous = slot.borrow_mut().replace(Box::new(hook));
        assert!(previous.is_none(), "increment-parse hook already installed");
    });
}

impl crate::cache::Cache {
    /// Returns true if a full build has ever stamped this cache (a
    /// `last_full_rebuild_ts` meta row exists). Fresh caches return false; used
    /// by the freshness probe to report an unbuilt cache as Stale.
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

    /// Full build: walk the vault, parse every document, replace all rows. The
    /// one-shot warm-up run at owner summon.
    pub fn full_build(&mut self, vault_root: &Utf8Path) -> Result<IndexReport, CacheError> {
        let start = Instant::now();
        let options = crate::graph::IndexOptions {
            ignore: self.files_ignore.clone(),
            alias_field: self.alias_field.clone(),
        };
        let index = crate::graph::build_index_with_options(vault_root, &options)?;

        let index_set = self.index_set.clone();
        let alias_field = self.alias_field.clone();
        let tx = self.conn.transaction()?;
        clear_all_rows(&tx)?;
        let mut report = IndexReport::default();
        for doc in &index.documents {
            insert_document(&tx, vault_root, doc, &mut report, &index_set)?;
        }
        for file in &index.files {
            insert_file(&tx, vault_root, file)?;
            report.file_count += 1;
        }
        update_meta_graph_fingerprint(&tx, &graph_fingerprint(&index))?;
        update_meta_rebuild_ts(&tx)?;
        update_meta_alias_field(&tx, alias_field.as_deref())?;
        update_meta_index_set_hash(&tx, &self.index_set_hash)?;
        tx.commit()?;
        // Refresh planner statistics for `document_fields` after a full rewrite,
        // scoped to that one table so it does not perturb existing planner
        // decisions for `links`/`documents` (see the EXPLAIN-guard tests).
        self.conn.execute("ANALYZE document_fields", [])?;

        report.duration_ms = start.elapsed().as_millis();
        Ok(report)
    }

    /// Incremental refresh: detect changes against the cached state, re-parse the
    /// whole vault for authority, drop+reinsert the affected documents, then
    /// re-resolve the entire links table. Defers to [`full_build`](Self::full_build)
    /// when the cache has never been built (the cheap change-detector only walks
    /// `.md`, so attachments would be missed).
    pub fn index_incremental(
        &mut self,
        vault_root: &Utf8Path,
        options: &ChangeDetectOptions,
    ) -> Result<IndexReport, CacheError> {
        if !self.has_been_built()? {
            return self.full_build(vault_root);
        }
        let start = Instant::now();
        let outcome = detect(vault_root, self, options)?;
        let mut changes = outcome.changes;
        let rebaselines = outcome.rebaselines;
        if changes.is_empty() && rebaselines.is_empty() {
            return Ok(IndexReport::default());
        }
        if changes.is_empty() {
            // Rebaseline-only refresh: no content changed, so no re-parse or link
            // rewrite is needed — just re-stamp the `(mtime,size)` baseline of the
            // touched-but-unchanged files inside one transaction so the freshness
            // probe converges (otherwise it re-fires and re-hashes them forever).
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            apply_rebaselines(&tx, &rebaselines)?;
            tx.commit()?;
            return Ok(IndexReport {
                duration_ms: start.elapsed().as_millis(),
                ..Default::default()
            });
        }

        let graph_opts = crate::graph::IndexOptions {
            ignore: self.files_ignore.clone(),
            alias_field: self.alias_field.clone(),
        };
        let mut fresh_index = crate::graph::build_index_with_options(vault_root, &graph_opts)?;
        #[cfg(test)]
        run_after_increment_parse_hook();

        // Detection and the whole-vault parse are separate observations. Expand
        // the affected set to include every path+hash difference the parse
        // observed, so an unrelated document changed between the two cannot leave
        // stale rows beside a fresh graph.
        let cached_hashes = load_document_hashes(&self.conn)?;
        let fresh_hashes: std::collections::BTreeMap<_, _> = fresh_index
            .documents
            .iter()
            .map(|d| (d.path.clone(), d.hash.clone()))
            .collect();
        let mut affected_paths: std::collections::BTreeSet<_> =
            changes.iter().map(|c| c.path().to_owned()).collect();
        let all_paths: std::collections::BTreeSet<_> = cached_hashes
            .keys()
            .chain(fresh_hashes.keys())
            .cloned()
            .collect();
        for path in all_paths {
            if affected_paths.contains(&path) {
                continue;
            }
            let drift = match (cached_hashes.get(&path), fresh_hashes.get(&path)) {
                (None, Some(_)) => Some(FileChange::Added(path.clone())),
                (Some(_), None) => Some(FileChange::Deleted(path.clone())),
                (Some(c), Some(f)) if c != f => Some(FileChange::Modified(path.clone())),
                _ => None,
            };
            if let Some(drift) = drift {
                affected_paths.insert(path);
                changes.push(drift);
            }
        }
        changes.sort_by(|a, b| a.path().cmp(b.path()));

        // Overlay: keep cached file identity for unaffected paths, take the fresh
        // parse for affected ones, then resolve links against that composite.
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

        let fresh_docs: HashMap<_, _> = fresh_index
            .documents
            .iter()
            .enumerate()
            .map(|(i, d)| (d.path.clone(), i))
            .collect();
        let fresh_files: HashMap<_, _> = fresh_index
            .files
            .iter()
            .enumerate()
            .map(|(i, f)| (f.path.clone(), i))
            .collect();

        let index_set = self.index_set.clone();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut report = IndexReport::default();

        // Publication-time drift re-proof INSIDE the commit transaction: re-read
        // each affected source and bind the committed `(mtime,size)` to a hash
        // that still matches the parse. A mismatch aborts the whole refresh (the
        // next probe re-fires) rather than committing stale content under a fresh
        // baseline.
        let mut authoritative_metadata: HashMap<Utf8PathBuf, (i64, i64)> = HashMap::new();
        for path in &affected_paths {
            match verify_affected_metadata(
                vault_root,
                path,
                &fresh_index,
                &fresh_docs,
                &fresh_files,
            ) {
                Ok(Some(observed)) => {
                    authoritative_metadata.insert(path.clone(), observed);
                }
                Ok(None) => {}
                Err(error) => {
                    tx.rollback()?;
                    return Err(error);
                }
            }
        }

        for change in &changes {
            let path = change.path();
            let observed = authoritative_metadata.get(path).copied();
            tx.execute("DELETE FROM files WHERE path = ?", [path.as_str()])?;
            if let Some(&file_i) = fresh_files.get(path) {
                let (mtime_ns, size_bytes) = observed.unwrap_or((0, 0));
                insert_file_with_metadata(&tx, &fresh_index.files[file_i], mtime_ns, size_bytes)?;
            }
            crate::cache::invalidation::drop_document(&tx, path)?;
            if let Some(&doc_i) = fresh_docs.get(path) {
                let (mtime_ns, size_bytes) = observed.unwrap_or((0, 0));
                insert_document_with_metadata(
                    &tx,
                    &fresh_index.documents[doc_i],
                    &mut report,
                    &index_set,
                    mtime_ns,
                    size_bytes,
                )?;
            }
        }

        // Converge the baseline of touched-but-unchanged files in the same commit,
        // so a mixed refresh (real changes + metadata drift) leaves the probe with
        // nothing to re-fire on. These rows are not in the affected set, so this
        // never conflicts with the drop+reinsert above.
        apply_rebaselines(&tx, &rebaselines)?;

        // Whole-table link re-resolution: link resolution is a global function of
        // the document set (an alias/path/stem change re-resolves links in
        // unchanged files too), so rewriting the whole table from the fresh index
        // is what makes an incremental refresh identical to a full build.
        rerun_link_resolution(&tx, &fresh_index)?;
        update_meta_graph_fingerprint(&tx, &graph_fingerprint(&fresh_index))?;
        tx.commit()?;

        report.duration_ms = start.elapsed().as_millis();
        Ok(report)
    }
}

// ── Chunked mutation-increment pipeline (ADR 0013/0014, DORMANT here) ────────

/// The wall-time budget one increment-commit chunk aims to stay within before
/// yielding to the writer queue's liveness class (ADR 0013). A chunk always
/// stages at least one WHOLE file or link, then stops once this budget is spent.
pub(crate) const INCREMENT_CHUNK_BUDGET: Duration = Duration::from_millis(50);

const INCREMENT_CHUNK_BUDGET_ENV: &str = "NORN_CACHE_INCREMENT_BUDGET_MS";

/// The increment-commit chunk budget. Release builds always return the 50ms
/// const; debug builds honor `NORN_CACHE_INCREMENT_BUDGET_MS` (accepts `0`).
pub(crate) fn increment_chunk_budget() -> Duration {
    crate::cache::engine::debug_env_duration_ms(
        INCREMENT_CHUNK_BUDGET_ENV,
        INCREMENT_CHUNK_BUDGET,
        /* accept_zero = */ true,
    )
}

/// Publication authority reserved on the write connection before the request
/// thread parses the vault (ADR 0014). The TEMP job marker carries the external
/// publication guard and lets a same-connection refresh supersede the work.
#[derive(Debug)]
pub(crate) struct IncrementReservation {
    job_id: i64,
    publication_epoch: u64,
}

enum IncrementPhase {
    Files,
    Links,
    Ready,
    Done,
    Superseded,
}

/// Driver state for a chunked increment commit. Built once at op start by
/// [`Cache::begin_increment_commit`] (which parses the whole vault WITHOUT a
/// lock), then advanced one chunk at a time by
/// [`Cache::commit_increment_chunk`].
pub(crate) struct IncrementCommit {
    job_id: i64,
    publication_epoch: u64,
    fresh_index: GraphIndex,
    fresh_docs: HashMap<Utf8PathBuf, usize>,
    fresh_files: HashMap<Utf8PathBuf, usize>,
    publication_fingerprint: String,
    staged_metadata: HashMap<Utf8PathBuf, (i64, i64)>,
    /// The affected paths, retained for the terminal-publish drift re-proof.
    affected_paths: std::collections::BTreeSet<Utf8PathBuf>,
    pending: VecDeque<Utf8PathBuf>,
    pending_links: VecDeque<(usize, usize)>,
    next_link_sequence: i64,
    phase: IncrementPhase,
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

impl crate::cache::Cache {
    /// Reserve a job marker and its external-publication baseline on the write
    /// connection (ADR 0014). `data_version` is captured before the O(1)
    /// graph-fingerprint check: a publication before the check changes the
    /// fingerprint, one after it changes the terminal data version.
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

    /// Begin a chunked increment commit for an explicit set of changed
    /// vault-relative paths, without a detect scan. Parses the whole vault in
    /// memory (the same "aggressive invalidation" authority the refresh uses),
    /// overlays the affected paths onto `baseline`, resolves links, and captures
    /// the metadata staging needs. No rows are written here.
    ///
    /// An associated function (no `&self`): the parse is read-only over the
    /// filesystem, so the owner can build the commit on the REQUEST thread (off
    /// the writer thread) and hand the driver to
    /// [`commit_increment_chunk`](Self::commit_increment_chunk).
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
            alias_field: alias_field.map(str::to_string),
        };
        let disk_index = crate::graph::build_index_with_options(vault_root, &options)?;

        let mut pending: Vec<Utf8PathBuf> = changed_paths.to_vec();
        pending.sort();
        pending.dedup();
        let affected_paths: std::collections::BTreeSet<_> = pending.iter().cloned().collect();

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
            .map(|(i, f)| (f.path.clone(), i))
            .collect();

        // Drift re-proof for the affected paths: bind staged `(mtime,size)` to a
        // re-read hash that still matches the parse (the same guard the terminal
        // publish re-applies). A mismatch aborts the increment.
        let mut staged_metadata = HashMap::new();
        for path in &affected_paths {
            if let Some(observed) =
                verify_affected_metadata(vault_root, path, &fresh_index, &fresh_docs, &fresh_files)?
            {
                staged_metadata.insert(path.clone(), observed);
            }
        }

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
            publication_fingerprint,
            staged_metadata,
            affected_paths,
            pending: pending.into_iter().collect(),
            pending_links,
            next_link_sequence: 0,
            phase: IncrementPhase::Files,
        })
    }

    /// Best-effort reservation cleanup when parsing fails before a driver exists.
    pub(crate) fn discard_increment_reservation(
        &mut self,
        reservation: &IncrementReservation,
    ) -> Result<(), CacheError> {
        self.cleanup_staged_increment(reservation.job_id)
    }

    /// Advance ONE chunk of an increment. Bulk chunks write only job-scoped TEMP
    /// rows and return `Ok(true)` so the scheduler can run liveness work between
    /// chunks. The terminal entry atomically replaces affected main rows plus all
    /// links. Returns `Ok(false)` after publication or supersession.
    pub(crate) fn commit_increment_chunk(
        &mut self,
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
    /// publishing.
    pub(crate) fn supersede_staged_increments_after_refresh(&mut self) {
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
        let index_set = self.index_set.clone();
        let tx = self.conn.transaction()?;
        let start = Instant::now();
        while let Some(path) = commit.pending.pop_front() {
            tx.execute(
                "INSERT INTO temp.norn_increment_paths (job_id, path) VALUES (?, ?)",
                params![commit.job_id, path.as_str()],
            )?;
            let meta = commit.staged_metadata.get(&path).copied();
            if let Some(&i) = commit.fresh_docs.get(&path) {
                let (mtime_ns, size_bytes) = meta.unwrap_or((0, 0));
                stage_document(
                    &tx,
                    commit.job_id,
                    &commit.fresh_index.documents[i],
                    &index_set,
                    mtime_ns,
                    size_bytes,
                )?;
            }
            if let Some(&i) = commit.fresh_files.get(&path) {
                let (mtime_ns, size_bytes) = meta.unwrap_or((0, 0));
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
        // The data_version guard defends internal concurrent publication (ADR
        // 0014): a publication between reservation and here bumps main's
        // data_version, so this job cleanly supersedes rather than clobbering.
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

        // Terminal drift re-proof INSIDE the publish transaction: re-read each
        // affected source, require the hash still match the parse, and overwrite
        // the staged `(mtime,size)` with the terminal stable observation so the
        // committed baseline can never encode content that changed after staging.
        for path in &commit.affected_paths {
            let observed = match verify_affected_metadata(
                &self.vault_root,
                path,
                &commit.fresh_index,
                &commit.fresh_docs,
                &commit.fresh_files,
            ) {
                Ok(observed) => observed,
                Err(error) => {
                    tx.rollback()?;
                    return Err(error);
                }
            };
            if let Some((mtime_ns, size_bytes)) = observed {
                tx.execute(
                    "UPDATE temp.norn_increment_files SET mtime_ns = ?, size_bytes = ? \
                     WHERE job_id = ? AND path = ?",
                    params![mtime_ns, size_bytes, job_id, path.as_str()],
                )?;
                tx.execute(
                    "UPDATE temp.norn_increment_documents SET mtime_ns = ?, size_bytes = ? \
                     WHERE job_id = ? AND path = ?",
                    params![mtime_ns, size_bytes, job_id, path.as_str()],
                )?;
            }
        }

        tx.execute("DELETE FROM main.links", [])?;
        for table in ["diagnostics", "block_ids", "headings", "document_fields"] {
            let key_col = if table == "document_fields" {
                "path"
            } else {
                "doc_path"
            };
            tx.execute(
                &format!(
                    "DELETE FROM main.{table} WHERE {key_col} IN \
                     (SELECT path FROM temp.norn_increment_paths WHERE job_id = ?)"
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
        // This publication supersedes every reservation captured before it.
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
    mtime_ns: i64,
    size_bytes: i64,
) -> Result<(), CacheError> {
    let frontmatter_json = doc
        .frontmatter
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default());
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
            mtime_ns,
            size_bytes,
        ],
    )?;

    let mut sequence = 0_i64;
    crate::cache::eav::visit_expanded_rows(
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
        let severity = severity_str(diagnostic.severity);
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

fn stage_link(tx: &Transaction, job_id: i64, sequence: i64, link: &Link) -> Result<(), CacheError> {
    let resolved = link.resolved_path.as_ref().map(|p| p.as_str());
    let source_context = link
        .source_context
        .as_ref()
        .map(|c| link_source_area_str(&c.area));
    let source_context_property = link
        .source_context
        .as_ref()
        .and_then(|c| c.property.as_deref());
    let (span_start, span_end, span_line, span_column) = span_columns(link);
    let unresolved_reason = link.unresolved_reason.as_ref().map(unresolved_reason_str);
    let candidates_json = candidates_json(link);
    tx.execute(
        "INSERT INTO temp.norn_increment_links
         (job_id, sequence, source_path, raw, kind, target_raw, resolved_path,
          anchor, block_ref, label, source_span_start, source_span_end,
          source_span_line, source_span_column, source_context,
          source_context_property, status, unresolved_reason, candidates_json)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
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
        ],
    )?;
    Ok(())
}

// ── Shared row helpers ───────────────────────────────────────────────────────

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

fn canonical_extension(extension: Option<&str>) -> Option<&str> {
    extension.filter(|extension| !extension.is_empty())
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

fn rerun_link_resolution(tx: &Transaction, fresh_index: &GraphIndex) -> Result<(), CacheError> {
    tx.execute("DELETE FROM links", [])?;
    for doc in &fresh_index.documents {
        for link in &doc.links {
            insert_link(tx, link)?;
        }
    }
    Ok(())
}

/// Re-stamp the `documents.(mtime_ns, size_bytes)` baseline for touched-but-
/// unchanged files (`detect`'s hash-matched rebaselines). Each metadata pair is
/// bound to the observation that re-proved the content hash, so this converges
/// the freshness probe without touching any content-derived rows.
fn apply_rebaselines(
    tx: &Transaction,
    rebaselines: &[(Utf8PathBuf, (i64, i64))],
) -> Result<(), CacheError> {
    for (path, (mtime_ns, size_bytes)) in rebaselines {
        tx.execute(
            "UPDATE documents SET mtime_ns = ?, size_bytes = ? WHERE path = ?",
            params![mtime_ns, size_bytes, path.as_str()],
        )?;
    }
    Ok(())
}

fn clear_all_rows(tx: &Transaction) -> Result<(), CacheError> {
    for table in [
        "documents",
        "document_fields",
        "files",
        "links",
        "headings",
        "block_ids",
        "diagnostics",
    ] {
        tx.execute(&format!("DELETE FROM {table}"), [])?;
    }
    Ok(())
}

fn insert_document(
    tx: &Transaction,
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
    tx: &Transaction,
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

    crate::cache::eav::insert_rows(tx, doc.path.as_str(), doc.frontmatter.as_ref(), index_set)?;

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
    tx: &Transaction,
    doc_path: &str,
    diagnostic: &crate::domain::Diagnostic,
) -> Result<(), CacheError> {
    tx.execute(
        "INSERT INTO diagnostics (doc_path, severity, code, message, detail)
         VALUES (?, ?, ?, ?, ?)",
        params![
            doc_path,
            severity_str(diagnostic.severity),
            diagnostic.code,
            diagnostic.message,
            diagnostic.detail,
        ],
    )?;
    Ok(())
}

fn severity_str(severity: crate::domain::Severity) -> &'static str {
    match severity {
        crate::domain::Severity::Warning => "warning",
        crate::domain::Severity::Error => "error",
    }
}

fn link_kind_str(kind: &crate::domain::LinkKind) -> &'static str {
    match kind {
        crate::domain::LinkKind::Wikilink => "wikilink",
        crate::domain::LinkKind::Markdown => "markdown",
        crate::domain::LinkKind::Embed => "embed",
    }
}

fn link_status_str(status: &crate::domain::LinkStatus) -> &'static str {
    match status {
        crate::domain::LinkStatus::Resolved => "resolved",
        crate::domain::LinkStatus::Unresolved => "unresolved",
        crate::domain::LinkStatus::Ambiguous => "ambiguous",
    }
}

fn link_source_area_str(area: &crate::domain::LinkSourceArea) -> &'static str {
    match area {
        crate::domain::LinkSourceArea::Body => "body",
        crate::domain::LinkSourceArea::Frontmatter => "frontmatter",
    }
}

fn unresolved_reason_str(reason: &crate::domain::UnresolvedReason) -> &'static str {
    match reason {
        crate::domain::UnresolvedReason::TargetMissing => "target-missing",
        crate::domain::UnresolvedReason::AnchorMissing => "anchor-missing",
        crate::domain::UnresolvedReason::BlockRefMissing => "block-ref-missing",
        crate::domain::UnresolvedReason::Ambiguous => "ambiguous",
    }
}

type SpanColumns = (Option<i64>, Option<i64>, Option<i64>, Option<i64>);

fn span_columns(link: &Link) -> SpanColumns {
    match &link.source_span {
        Some(s) => (
            Some(s.byte_offset as i64),
            None,
            Some(s.line as i64),
            Some(s.column as i64),
        ),
        None => (None, None, None, None),
    }
}

fn candidates_json(link: &Link) -> Option<String> {
    if link.candidates.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&link.candidates).unwrap_or_default())
    }
}

fn insert_link(tx: &Transaction, link: &Link) -> Result<(), CacheError> {
    let resolved = link.resolved_path.as_ref().map(|p| p.as_str().to_string());
    let source_context = link
        .source_context
        .as_ref()
        .map(|c| link_source_area_str(&c.area).to_string());
    let source_context_property = link
        .source_context
        .as_ref()
        .and_then(|c| c.property.clone());
    let (span_start, span_end, span_line, span_column) = span_columns(link);
    let unresolved_reason = link.unresolved_reason.as_ref().map(unresolved_reason_str);
    let candidates_json = candidates_json(link);
    // `prepare_cached`: link insertion is a hot per-link loop
    // (`rerun_link_resolution` rewrites the whole links table every increment).
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

fn insert_file(
    tx: &Transaction,
    vault_root: &Utf8Path,
    file: &VaultFile,
) -> Result<(), CacheError> {
    let absolute = vault_root.join(&file.path);
    let size = size_bytes(&absolute).unwrap_or(0);
    let mtime = mtime_ns(&absolute).unwrap_or(0);
    insert_file_with_metadata(tx, file, mtime, size)
}

fn insert_file_with_metadata(
    tx: &Transaction,
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

fn update_meta_rebuild_ts(tx: &Transaction) -> Result<(), CacheError> {
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

fn update_meta_alias_field(tx: &Transaction, alias_field: Option<&str>) -> Result<(), CacheError> {
    tx.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('links_alias_field', ?)",
        params![alias_field.unwrap_or("")],
    )?;
    Ok(())
}

fn update_meta_index_set_hash(tx: &Transaction, hash: &str) -> Result<(), CacheError> {
    tx.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('index_set_hash', ?)",
        params![hash],
    )?;
    Ok(())
}

fn update_meta_graph_fingerprint(tx: &Transaction, fingerprint: &str) -> Result<(), CacheError> {
    tx.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('graph_fingerprint', ?)",
        params![fingerprint],
    )?;
    Ok(())
}

/// Re-prove that the on-disk source at `path` still holds the exact content the
/// parse hashed, returning the `(mtime_ns, size_bytes)` from that same stable
/// observation. Stats before and after the read must agree (no write raced the
/// read itself), the byte length must match the terminal size, and the re-read
/// blake3 hash must equal `expected_hash`. `None` on any mismatch — the caller
/// aborts the commit with [`CacheError::IncrementSourceDrift`], never publishing
/// stale content under a fresh baseline.
fn verify_parsed_document(
    vault_root: &Utf8Path,
    path: &Utf8Path,
    expected_hash: &str,
) -> Option<(i64, i64)> {
    let absolute = vault_root.join(path);
    let before = regular_file_metadata(&absolute)?;
    let bytes = std::fs::read(absolute.as_std_path()).ok()?;
    let after = regular_file_metadata(&absolute)?;
    (before == after
        && bytes.len() as i64 == after.1
        && blake3::hash(&bytes).to_hex().as_str() == expected_hash)
        .then_some(after)
}

/// A regular file's `(mtime_ns, size_bytes)` observed twice with an equality
/// guard, for non-document sources (attachments) that carry no parsed hash.
fn stable_regular_file_metadata(vault_root: &Utf8Path, path: &Utf8Path) -> Option<(i64, i64)> {
    let absolute = vault_root.join(path);
    let before = regular_file_metadata(&absolute)?;
    let after = regular_file_metadata(&absolute)?;
    (before == after).then_some(after)
}

fn regular_file_metadata(absolute: &Utf8Path) -> Option<(i64, i64)> {
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

/// Re-prove the committed `(mtime,size)` for one affected path against the fresh
/// parse: a parsed document (non-empty hash) must still hash to its parsed
/// content; a non-document regular file must be stat-stable; a read-failed
/// document (empty hash) commits the impossible `(-1, -1)` force-refire sentinel
/// so the next probe always re-fires on it; a deleted/absent path carries no
/// metadata (`Ok(None)`). A drift on a source that IS expected present and parsed
/// is a hard abort.
fn verify_affected_metadata(
    vault_root: &Utf8Path,
    path: &Utf8Path,
    fresh_index: &GraphIndex,
    fresh_docs: &HashMap<Utf8PathBuf, usize>,
    fresh_files: &HashMap<Utf8PathBuf, usize>,
) -> Result<Option<(i64, i64)>, CacheError> {
    if let Some(&doc_i) = fresh_docs.get(path) {
        let doc = &fresh_index.documents[doc_i];
        if doc.hash.is_empty() {
            // Read-failed document (no parsed content hash to re-prove): commit
            // the impossible `(-1, -1)` metadata sentinel so the next
            // freshness/detect pass ALWAYS refires on this path. A best-effort
            // real stat would let a metadata-only recovery that leaves mtime+size
            // unchanged (e.g. a `chmod` that touches only ctime) keep serving the
            // stale read-failed row as Fresh — the stat-sweep probe compares
            // mtime+size and never notices. `(-1, -1)` can never equal a live
            // stat, forcing the retry after permission/readability recovery.
            return Ok(Some((-1, -1)));
        }
        let observed = verify_parsed_document(vault_root, path, &doc.hash).ok_or_else(|| {
            CacheError::IncrementSourceDrift {
                path: path.to_owned(),
            }
        })?;
        return Ok(Some(observed));
    }
    if fresh_files.contains_key(path) {
        let observed = stable_regular_file_metadata(vault_root, path).ok_or_else(|| {
            CacheError::IncrementSourceDrift {
                path: path.to_owned(),
            }
        })?;
        return Ok(Some(observed));
    }
    // Deletion / graph-excluded path: nothing to publish, nothing to verify.
    Ok(None)
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
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    use crate::cache::{Cache, CacheError};

    #[derive(Debug, Clone)]
    enum Op {
        Create(String),
        Modify(String),
        Delete(String),
    }

    fn fresh_vault() -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        (tmp, root)
    }

    fn apply_op(root: &camino::Utf8Path, op: &Op) {
        match op {
            Op::Create(name) => {
                std::fs::write(
                    root.join(format!("{name}.md")).as_std_path(),
                    format!("---\ntitle: {name}\n---\nbody [link]({name}-target.md)\n"),
                )
                .unwrap();
            }
            Op::Modify(name) => {
                std::fs::write(
                    root.join(format!("{name}.md")).as_std_path(),
                    format!("---\ntitle: {name}\n---\nupdated body\n"),
                )
                .unwrap();
            }
            Op::Delete(name) => {
                let _ = std::fs::remove_file(root.join(format!("{name}.md")).as_std_path());
            }
        }
    }

    /// Any sequence of filesystem ops must produce the same final cache state via
    /// incremental refresh as a from-scratch full build.
    fn run_sequence(ops: &[Op]) {
        let (_tmp1, root1) = fresh_vault();
        let (_tmp2, root2) = fresh_vault();

        for op in ops {
            apply_op(&root1, op);
            apply_op(&root2, op);
            let mut cache1 = Cache::open(&root1).unwrap();
            cache1
                .index_incremental(&root1, &Default::default())
                .unwrap();
        }

        let mut cache2 = Cache::open(&root2).unwrap();
        cache2.full_build(&root2).unwrap();

        let cache1 = Cache::open(&root1).unwrap();
        let index1 = cache1.load_graph_index().unwrap();
        let index2 = cache2.load_graph_index().unwrap();

        let paths1: std::collections::BTreeSet<_> =
            index1.documents.iter().map(|d| d.path.clone()).collect();
        let paths2: std::collections::BTreeSet<_> =
            index2.documents.iter().map(|d| d.path.clone()).collect();
        assert_eq!(paths1, paths2, "path set drift; ops: {ops:?}");

        let links1: usize = index1.documents.iter().map(|d| d.links.len()).sum();
        let links2: usize = index2.documents.iter().map(|d| d.links.len()).sum();
        assert_eq!(links1, links2, "link count drift; ops: {ops:?}");
    }

    #[test]
    fn incremental_matches_from_scratch_simple() {
        run_sequence(&[
            Op::Create("a".into()),
            Op::Create("b".into()),
            Op::Modify("a".into()),
            Op::Delete("b".into()),
        ]);
    }

    #[test]
    fn incremental_matches_from_scratch_create_delete_create() {
        run_sequence(&[
            Op::Create("foo".into()),
            Op::Delete("foo".into()),
            Op::Create("foo".into()),
        ]);
    }

    #[test]
    fn incremental_matches_from_scratch_interleaved() {
        let mut ops = Vec::new();
        for i in 0..10 {
            ops.push(Op::Create(format!("doc{i}")));
            if i % 2 == 0 {
                ops.push(Op::Modify(format!("doc{i}")));
            }
            if i % 3 == 0 && i > 0 {
                ops.push(Op::Delete(format!("doc{}", i - 1)));
            }
        }
        run_sequence(&ops);
    }

    /// The chunked mutation-increment pipeline (dormant): stage a new document
    /// through reserve → begin → chunk → publish and confirm it lands.
    #[test]
    fn chunked_increment_publishes_a_new_document() {
        let (_tmp, root) = fresh_vault();
        std::fs::write(root.join("a.md").as_std_path(), "---\ntitle: A\n---\n").unwrap();
        let mut cache = Cache::open(&root).unwrap();
        cache.full_build(&root).unwrap();

        // Add a new file, then drive the increment for it.
        std::fs::write(
            root.join("b.md").as_std_path(),
            "---\ntitle: B\n---\nsee [[a]]\n",
        )
        .unwrap();
        let baseline = cache.load_graph_index().unwrap();
        let fingerprint: String = cache
            .conn()
            .query_row(
                "SELECT value FROM meta WHERE key = 'graph_fingerprint'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let reservation = cache.reserve_increment_commit(&fingerprint).unwrap();
        let mut commit = Cache::begin_increment_commit(
            &root,
            &[Utf8PathBuf::from("b.md")],
            None,
            &[],
            &reservation,
            baseline,
        )
        .unwrap();
        let budget = std::time::Duration::from_millis(0);
        while cache.commit_increment_chunk(&mut commit, budget).unwrap() {}

        let loaded = cache.load_graph_index().unwrap();
        let paths: std::collections::BTreeSet<_> =
            loaded.documents.iter().map(|d| d.path.clone()).collect();
        assert!(paths.contains(&Utf8PathBuf::from("a.md")));
        assert!(
            paths.contains(&Utf8PathBuf::from("b.md")),
            "increment should have published b.md; got {paths:?}"
        );
    }

    /// F1 — index_incremental: a file rewritten AFTER the parse but before the
    /// commit's drift re-proof must abort the whole refresh, never committing
    /// the stale-content-under-fresh-baseline that the stat-sweep probe would
    /// then read as Fresh forever.
    #[test]
    fn incremental_aborts_on_source_drift_between_parse_and_commit() {
        let (_tmp, root) = fresh_vault();
        std::fs::write(root.join("a.md").as_std_path(), "---\ntitle: A\n---\n").unwrap();
        let mut cache = Cache::open(&root).unwrap();
        cache.full_build(&root).unwrap();

        // A new file lands; its parse will hash content v1.
        std::fs::write(
            root.join("b.md").as_std_path(),
            "---\ntitle: B\n---\nversion one\n",
        )
        .unwrap();

        // Race a DIFFERENT write into the window between the parse and the
        // in-transaction re-read.
        let racing_path = root.join("b.md");
        super::install_after_increment_parse_hook(move || {
            std::fs::write(
                racing_path.as_std_path(),
                "---\ntitle: B\n---\nversion TWO, different bytes\n",
            )
            .unwrap();
        });

        let result = cache.index_incremental(&root, &Default::default());
        assert!(
            matches!(result, Err(CacheError::IncrementSourceDrift { .. })),
            "expected IncrementSourceDrift, got {result:?}"
        );

        // The refresh aborted: b.md was never committed, so the cache still holds
        // only a.md. The next probe re-fires on b.md's still-newer stat.
        let paths: std::collections::BTreeSet<_> = cache
            .load_graph_index()
            .unwrap()
            .documents
            .iter()
            .map(|d| d.path.clone())
            .collect();
        assert_eq!(
            paths,
            std::collections::BTreeSet::from([Utf8PathBuf::from("a.md")]),
            "stale content must not be committed on drift"
        );
    }

    /// F1 — chunked increment: a file rewritten AFTER `begin_increment_commit`
    /// staged its v1 rows but before the terminal publish must be caught by the
    /// publish-transaction re-proof, aborting the commit.
    #[test]
    fn chunked_increment_aborts_on_source_drift_before_terminal_publish() {
        let (_tmp, root) = fresh_vault();
        std::fs::write(root.join("a.md").as_std_path(), "---\ntitle: A\n---\n").unwrap();
        let mut cache = Cache::open(&root).unwrap();
        cache.full_build(&root).unwrap();

        std::fs::write(
            root.join("b.md").as_std_path(),
            "---\ntitle: B\n---\nversion one\n",
        )
        .unwrap();
        let baseline = cache.load_graph_index().unwrap();
        let fingerprint: String = cache
            .conn()
            .query_row(
                "SELECT value FROM meta WHERE key = 'graph_fingerprint'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let reservation = cache.reserve_increment_commit(&fingerprint).unwrap();
        let mut commit = Cache::begin_increment_commit(
            &root,
            &[Utf8PathBuf::from("b.md")],
            None,
            &[],
            &reservation,
            baseline,
        )
        .unwrap();

        // Concurrent external write after staging captured v1's content.
        std::fs::write(
            root.join("b.md").as_std_path(),
            "---\ntitle: B\n---\nversion TWO\n",
        )
        .unwrap();

        let budget = std::time::Duration::from_millis(0);
        let mut result: Result<bool, CacheError> = Ok(true);
        while let Ok(true) = result {
            result = cache.commit_increment_chunk(&mut commit, budget);
        }
        assert!(
            matches!(result, Err(CacheError::IncrementSourceDrift { .. })),
            "terminal publish must re-prove drift, got {result:?}"
        );

        // b.md was never published under a stale baseline.
        let paths: std::collections::BTreeSet<_> = cache
            .load_graph_index()
            .unwrap()
            .documents
            .iter()
            .map(|d| d.path.clone())
            .collect();
        assert_eq!(
            paths,
            std::collections::BTreeSet::from([Utf8PathBuf::from("a.md")])
        );
    }

    /// F1 residual — a read-failed document commits the impossible `(-1,-1)`
    /// force-refire baseline (never a real stat), so a subsequent freshness probe
    /// reports it Stale even when its live mtime+size are unchanged. Without the
    /// sentinel, a metadata-only readability recovery that leaves stat untouched
    /// would keep serving the stale read-failed row as Fresh forever.
    #[test]
    fn read_failed_document_commits_force_refire_sentinel() {
        use crate::cache::freshness::{Freshness, FreshnessProbe, StatSweepProbe};

        let (_tmp, root) = fresh_vault();
        std::fs::write(root.join("a.md").as_std_path(), "---\ntitle: A\n---\n").unwrap();
        let mut cache = Cache::open(&root).unwrap();
        cache.full_build(&root).unwrap();

        // A .md whose bytes are not valid UTF-8: build_index records a read-failed
        // Document with an empty content hash.
        std::fs::write(
            root.join("bad.md").as_std_path(),
            b"\xff\xfe\xfd\xfc not valid utf-8",
        )
        .unwrap();
        cache.index_incremental(&root, &Default::default()).unwrap();

        // The committed baseline for the read-failed doc is the impossible
        // sentinel, not a real stat.
        let (mtime, size): (i64, i64) = cache
            .conn()
            .query_row(
                "SELECT mtime_ns, size_bytes FROM documents WHERE path = 'bad.md'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (mtime, size),
            (-1, -1),
            "read-failed doc must commit the (-1,-1) force-refire sentinel"
        );

        // The probe reports Stale for that path despite its live stats never
        // matching the sentinel — the retry-after-recovery guarantee.
        match StatSweepProbe.probe(&root, &cache).unwrap() {
            Freshness::Stale(_) => {}
            Freshness::Fresh => panic!("read-failed sentinel must force the next probe to refire"),
        }
    }

    /// A touched-but-unchanged file (same bytes, bumped mtime) must converge after
    /// ONE incremental refresh: the rebaseline re-stamps its `(mtime,size)` so the
    /// probe reports Fresh again, instead of re-firing (and re-hashing the file)
    /// on every subsequent request forever.
    #[test]
    fn touched_but_unchanged_file_converges_after_one_refresh() {
        use crate::cache::freshness::{Freshness, FreshnessProbe, StatSweepProbe};

        let (_tmp, root) = fresh_vault();
        std::fs::write(
            root.join("a.md").as_std_path(),
            "---\ntitle: A\n---\nbody\n",
        )
        .unwrap();
        let mut cache = Cache::open(&root).unwrap();
        cache.full_build(&root).unwrap();
        assert_eq!(
            StatSweepProbe.probe(&root, &cache).unwrap(),
            Freshness::Fresh
        );

        // Same bytes, new mtime → the probe now reads Stale.
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(
            root.join("a.md").as_std_path(),
            "---\ntitle: A\n---\nbody\n",
        )
        .unwrap();
        assert!(
            matches!(
                StatSweepProbe.probe(&root, &cache).unwrap(),
                Freshness::Stale(_)
            ),
            "a bumped mtime must read Stale before the rebaseline"
        );

        // One refresh (rebaseline-only) must converge the baseline.
        let report = cache.index_incremental(&root, &Default::default()).unwrap();
        assert_eq!(
            report.doc_count, 0,
            "no content changed, so nothing re-indexed"
        );

        assert_eq!(
            StatSweepProbe.probe(&root, &cache).unwrap(),
            Freshness::Fresh,
            "the rebaseline must converge the probe — no permanent refresh loop"
        );
        // Idempotent: a second refresh sees nothing to do.
        let again = cache.index_incremental(&root, &Default::default()).unwrap();
        assert_eq!(again.doc_count, 0);
        assert_eq!(
            StatSweepProbe.probe(&root, &cache).unwrap(),
            Freshness::Fresh
        );
    }
}
