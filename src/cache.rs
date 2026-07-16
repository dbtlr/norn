//! SQLite-backed cache for the vault graph.
//!
//! Acts as the read path for query commands: `Cache::load_graph_index` returns
//! the same `GraphIndex` shape that `vault-graph::build_index` does, but loads
//! from SQLite instead of walking the filesystem.
//!
//! The cache is *disposable*. Missing, corrupted, schema-mismatched, or
//! identity-drifted caches trigger a silent rebuild rather than erroring.

mod error;
mod live_examples;
mod query;

pub(crate) use error::CacheError;
pub(crate) use find::{FindQuery, FindResult, SortClause, SortDirection};
pub(crate) use live_examples::{count_matching, field_statistics, FieldStats};
pub(crate) use query::DocumentQuery;

pub(crate) mod canonical;
mod change_detection;
pub(crate) mod channel;
mod document_fields;
#[cfg(test)]
mod eav_acceptance;
mod find;
mod freshness;
mod identity;
mod invalidation;
mod lock;
mod open;
pub(crate) mod prune;
mod query_diagnostics;
mod query_documents;
mod query_links;
mod query_show;
mod reader;
#[cfg(test)]
pub(crate) use reader::install_after_documents_loaded_hook;
#[cfg(test)]
mod scan_semantics_probe;
pub(crate) mod schema;
mod status;
mod writer;

pub(crate) use change_detection::ChangeDetectOptions;
pub(crate) use freshness::{Freshness, FreshnessProbe, StatSweepProbe};
#[cfg(unix)]
pub(crate) use identity::canonical_vault_identity_hash;
pub(crate) use identity::{
    cache_dir_for, cache_tree_root, events_dir_for, hex_lower, state_dir_for, state_tree_root,
    vault_identity, vault_identity_hash, xdg_cache_home_env,
};
#[cfg(test)]
pub(crate) use writer::IndexReport;
pub(crate) use writer::{
    graph_fingerprint, increment_chunk_budget, IncrementCommit, IncrementReservation,
};

/// Resolve a vault's on-disk cache directory under an EXPLICIT cache home,
/// with the SAME identity mapping production opens use (`identity::cache_dir_in`,
/// which `cache_dir_for` delegates to after reading `XDG_CACHE_HOME`), returning
/// just the cache dir. Cross-crate test-harness seam: the NRN-83 acceptance
/// benchmark locates `cache.db` under a private cache home through it — a pure
/// function of its arguments, no process-env read or mutation anywhere. The
/// error is stringified so the crate-private `CacheError` type does not leak
/// into public visibility. Not part of the stable public API — same posture as
/// `Cache::conn`.
#[doc(hidden)]
pub fn resolve_cache_dir_in(
    cache_home: &camino::Utf8Path,
    vault_root: &camino::Utf8Path,
) -> Result<camino::Utf8PathBuf, String> {
    identity::cache_dir_in(cache_home, vault_root)
        .map(|(_canonical, dir)| dir)
        .map_err(|e| e.to_string())
}

/// Sibling of [`resolve_cache_dir_in`] for the vault ENTRY dir — the
/// channel-independent `<cache_home>/norn/<hash>` directory holding the shared
/// write lock (`.lock`), which is the entry dir itself on the live channel and
/// the parent of the `dev/` db dir on the dev channel (NRN-269). Cross-crate
/// test-harness seam: contention tests hold the vault's real write lock through
/// it instead of re-encoding the on-disk layout. Same posture as
/// [`resolve_cache_dir_in`]: pure function of its arguments, stringified error,
/// not part of the stable public API.
#[doc(hidden)]
pub fn resolve_cache_lock_dir_in(
    cache_home: &camino::Utf8Path,
    vault_root: &camino::Utf8Path,
) -> Result<camino::Utf8PathBuf, String> {
    identity::cache_layout_in(cache_home, vault_root)
        .map(|layout| layout.entry_dir)
        .map_err(|e| e.to_string())
}
pub(crate) use lock::{acquire_flock, debug_env_usize};
#[cfg(test)]
pub(crate) use query_show::set_after_document_row_hook;
pub(crate) use query_show::{DocumentDeep, IncomingLink};

pub(crate) const SCHEMA_VERSION: u32 = 5;

/// The single operator note emitted when the implicit incremental refresh cannot
/// acquire the write lock in time (`CacheError::LockTimeout`) and the query
/// proceeds against the current cache state. Shared by the direct read path
/// (`cache_cmd::load_graph_index` / `open_for_query`) and the warm daemon
/// (`mcp::context::query_cache_warm`) so the two surfaces cannot drift on the
/// wording, and so a routed read can forward this exact text to the CLI's stderr
/// byte-identically to a direct run (NRN-215).
pub(crate) const LOCK_CONTENTION_NOTE: &str =
    "vault: another cache operation is in progress; using current cache state";

/// Handle to an opened cache. Holds a rusqlite Connection plus the resolved
/// vault root and cache directory path. `alias_field` is the value passed
/// in via `Cache::open_with_config`; it gets written to the `links_alias_field`
/// meta row on every rebuild so subsequent opens can detect config drift.
/// `index_set`/`index_set_hash` are the resolved Wave-2 frontmatter-index
/// field set (see `crate::standards::index_policy::resolved_index_set`),
/// passed in via `Cache::open_with_index`; they drive what the
/// `document_fields` EAV writer indexes and are compared against the cached
/// `index_set_hash` meta row on open to decide whether to re-shred.
///
/// `index_authoritative` records whether `index_set`/`index_set_hash` came
/// from real config knowledge (`Cache::open_with_index`, `true`) or are just
/// the unconfigured-default empty set (`Cache::open` / `open_with_config`,
/// `false`). Only an authoritative open may reconcile `document_fields`
/// against `index_set` — see `document_fields::reshred_if_needed` — or stamp
/// the `index_set_hash` meta row on rebuild/incremental index. A
/// non-authoritative open simply doesn't know the operator's real declared
/// field set, so treating its empty default as ground truth would delete a
/// previously-shredded index and stamp a hash that doesn't reflect config.
pub(crate) struct Cache {
    pub(crate) conn: rusqlite::Connection,
    pub(crate) vault_root: camino::Utf8PathBuf,
    /// Directory holding this handle's `cache.db` — channel-specific: the vault
    /// entry dir on the live channel, its `dev/` subdir on the dev channel
    /// (NRN-269).
    pub(crate) cache_dir: camino::Utf8PathBuf,
    /// The channel-independent vault entry dir (`<cache_home>/norn/<hash>`) that
    /// holds the shared write lock (`.lock`). Equal to `cache_dir` on the live
    /// channel; `cache_dir`'s parent on the dev channel. A dev and a live binary
    /// against the same vault serialize on this one lock.
    pub(crate) lock_dir: camino::Utf8PathBuf,
    /// The cache channel this handle opened under, carried from the resolved
    /// [`identity::CacheLayout`] rather than re-derived from path geometry.
    pub(crate) channel: channel::Channel,
    pub(crate) alias_field: Option<String>,
    /// Compiled-out-of `files.ignore`: path globs whose matching files are
    /// excluded from the graph at cache-build time — never parsed, indexed, or
    /// link-resolvable (NRN-117, ADR 0007). Threaded in via `open_with_index`;
    /// empty for the non-authoritative `open`/`open_with_config` constructors,
    /// which have no config knowledge.
    pub(crate) files_ignore: Vec<String>,
    pub(crate) index_set: std::collections::BTreeSet<String>,
    pub(crate) index_set_hash: String,
    pub(crate) index_authoritative: bool,
    /// Same-connection publication authority for reserved incremental jobs.
    pub(crate) increment_publication_epoch: u64,
}

impl Cache {
    /// Run a structured cache read against one SQLite snapshot.
    ///
    /// Callers open/refresh the cache before entering this closure, so freshness
    /// work never extends the snapshot lifetime. `unchecked_transaction` accepts
    /// `&Connection`, allowing the existing read primitives to keep taking
    /// `&Cache`; this transaction is read-only by convention and remains
    /// WAL-friendly for concurrent writers. Nested calls reuse the outer
    /// transaction, allowing snapshot-owning helpers to compose into a wider
    /// structured read phase without a nested `BEGIN`.
    pub(crate) fn read_snapshot<T>(
        &self,
        read: impl FnOnce(&Self) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        use anyhow::Context as _;

        // Compose snapshot-owning helpers inside a wider structured read phase.
        // SQLite exposes the connection's transaction state, so an inner helper
        // can reuse the caller's snapshot without attempting a nested BEGIN.
        if !self.conn.is_autocommit() {
            return read(self);
        }

        let transaction = self
            .conn
            .unchecked_transaction()
            .context("failed to begin cache read snapshot")?;
        match read(self) {
            Ok(value) => {
                transaction
                    .commit()
                    .context("failed to close cache read snapshot")?;
                Ok(value)
            }
            Err(error) => {
                if let Err(rollback_error) = transaction.rollback() {
                    return Err(error.context(format!(
                        "failed to roll back cache read snapshot: {rollback_error}"
                    )));
                }
                Err(error)
            }
        }
    }

    /// Delete the on-disk cache (database + WAL/SHM siblings). Holds the
    /// advisory write lock for the duration. After clear the caller should
    /// drop the `Cache` handle; the next `Cache::open` recreates a fresh
    /// database with the current schema and identity meta rows.
    pub fn clear(&mut self) -> Result<(), CacheError> {
        let _lock = lock::WriteLock::acquire(&self.lock_dir, std::time::Duration::from_secs(5))?;
        let db_path = self.cache_dir.join("cache.db");
        // Detach the live connection from the on-disk database so the file
        // can be removed cleanly on platforms (notably Windows) where an
        // open handle blocks deletion. Replace with an in-memory connection
        // so `&mut self.conn` remains usable until the caller drops us.
        drop(std::mem::replace(
            &mut self.conn,
            rusqlite::Connection::open_in_memory()?,
        ));
        if db_path.as_std_path().exists() {
            std::fs::remove_file(db_path.as_std_path()).map_err(|e| CacheError::Io {
                path: db_path.clone(),
                source: e,
            })?;
        }
        let wal = self.cache_dir.join("cache.db-wal");
        let shm = self.cache_dir.join("cache.db-shm");
        let _ = std::fs::remove_file(wal.as_std_path());
        let _ = std::fs::remove_file(shm.as_std_path());
        Ok(())
    }

    /// Crate-internal connection accessor for production primitives and
    /// for tests (including cross-crate integration tests) that need direct
    /// SQL access. Not part of the stable public API — treat as `#[doc(hidden)]`.
    #[doc(hidden)]
    pub fn conn(&self) -> &rusqlite::Connection {
        &self.conn
    }

    /// The cache channel this handle opened under. Drives the
    /// `norn cache status` channel line.
    pub(crate) fn channel_label(&self) -> &'static str {
        self.channel.as_str()
    }

    /// Configured frontmatter field name used for alias parsing, if any.
    /// Returns `None` when the cache was opened without an alias field
    /// (i.e. via `Cache::open` or `open_with_config(_, None)`).
    pub fn alias_field(&self) -> Option<&str> {
        self.alias_field.as_deref()
    }

    /// Every distinct top-level frontmatter key observed across the vault's
    /// documents. Drives the dynamic-field-predicate universe (ADR 0010,
    /// NRN-207): a `--key value` guess is only accepted when `key` is a field
    /// this vault actually carries. Uses SQLite's JSON1 `json_each` over the
    /// cached `frontmatter_json`, so no filesystem re-parse is needed.
    pub fn observed_field_names(&self) -> Result<std::collections::BTreeSet<String>, CacheError> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT je.key \
             FROM documents, json_each(documents.frontmatter_json) je \
             WHERE documents.frontmatter_json IS NOT NULL \
               AND json_valid(documents.frontmatter_json) \
               AND json_type(documents.frontmatter_json) = 'object'",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut fields = std::collections::BTreeSet::new();
        for row in rows {
            fields.insert(row?);
        }
        Ok(fields)
    }
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    // Performance regression test. Locks in the documented cold-rebuild target:
    // a 1000-document vault should rebuild from scratch in under 2 seconds.
    //
    // Marked `#[ignore]` so it does not run on every `cargo test` invocation.
    // Opt in via `cargo test --ignored` or in CI when locking targets.
    #[test]
    #[ignore]
    fn cold_rebuild_under_2s_on_1k_docs() {
        let tmp = TempDir::new().unwrap();
        // Nest under `vault/` so the basename is not hidden — TempDir uses
        // `.tmp...` which `vault_graph` skips.
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        for i in 0..1000 {
            std::fs::write(
                root.join(format!("doc{i}.md")).as_std_path(),
                format!("---\ntitle: Doc {i}\n---\nbody\n"),
            )
            .unwrap();
        }
        let mut cache = crate::cache::Cache::open(&root).unwrap();
        let start = std::time::Instant::now();
        cache.rebuild(&root).unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() < 2000,
            "cold rebuild took {}ms (target: < 2000ms)",
            elapsed.as_millis(),
        );
    }

    // Property test: any sequence of filesystem operations must produce the
    // same final cache state via incremental update as from-scratch rebuild.
    //
    // Catches invalidation bugs that scenario tests miss by running random
    // sequences of (Create, Modify, Delete) ops against two parallel vaults
    // and asserting the indices match.
    mod property {
        use super::*;

        #[derive(Debug, Clone)]
        enum Op {
            Create(String),
            Modify(String),
            Delete(String),
        }

        /// Builds an isolated vault rooted at `<tmpdir>/vault/`. `vault_graph` treats
        /// directories whose basename starts with `.` as hidden, and `TempDir` itself
        /// uses a `.tmp...` prefix — so we nest under a non-hidden subdirectory.
        fn fresh_vault() -> (TempDir, Utf8PathBuf) {
            let tmp = TempDir::new().unwrap();
            let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
                .unwrap()
                .join("vault");
            std::fs::create_dir(root.as_std_path()).unwrap();
            (tmp, root)
        }

        fn run_sequence(ops: &[Op]) {
            let (_tmp1, root1) = fresh_vault();
            let (_tmp2, root2) = fresh_vault();

            // Apply ops to both vaults identically.
            // root1 gets an incremental update after each op.
            // root2 only gets a single from-scratch rebuild at the end.
            for op in ops {
                apply_op(&root1, op);
                apply_op(&root2, op);
                let mut cache1 = crate::cache::Cache::open(&root1).unwrap();
                cache1
                    .index_incremental(&root1, &Default::default())
                    .unwrap();
            }

            let mut cache2 = crate::cache::Cache::open(&root2).unwrap();
            cache2.rebuild(&root2).unwrap();

            let cache1 = crate::cache::Cache::open(&root1).unwrap();
            let index1 = cache1.load_graph_index().unwrap();
            let index2 = cache2.load_graph_index().unwrap();

            assert_eq!(
                index1.documents.len(),
                index2.documents.len(),
                "doc count drift: {} (incremental) vs {} (from-scratch); ops: {:?}",
                index1.documents.len(),
                index2.documents.len(),
                ops,
            );

            let paths1: std::collections::BTreeSet<_> =
                index1.documents.iter().map(|d| d.path.clone()).collect();
            let paths2: std::collections::BTreeSet<_> =
                index2.documents.iter().map(|d| d.path.clone()).collect();
            assert_eq!(paths1, paths2, "path set drift; ops: {:?}", ops);

            let links1: usize = index1.documents.iter().map(|d| d.links.len()).sum();
            let links2: usize = index2.documents.iter().map(|d| d.links.len()).sum();
            assert_eq!(
                links1, links2,
                "link count drift: {links1} (incremental) vs {links2} (from-scratch); ops: {ops:?}",
            );
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
        fn incremental_matches_from_scratch_many_creates() {
            let ops: Vec<Op> = (0..20).map(|i| Op::Create(format!("doc{i}"))).collect();
            run_sequence(&ops);
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
    }
}
