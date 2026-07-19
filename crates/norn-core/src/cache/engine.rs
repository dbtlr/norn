//! The cache engine handle: one opened SQLite connection plus the resolved
//! vault config it was opened under.
//!
//! # Lifecycle (ADR 0017)
//!
//! The db is pure derivation. The owner [`create`](Cache::create)s it at summon
//! (schema, then a one-shot [`full_build`](Cache::full_build) warm-up) and
//! deletes it at exit. No other process opens it. There is no identity path
//! matrix, no channel dirs, no self-heal ladder, no reshred-on-open: a
//! client-vs-cache schema mismatch is unrepresentable because the db is created
//! fresh under the current schema and config every summon.
//!
//! # Corruption = exit-to-heal
//!
//! Any corruption signal — a `rusqlite::Error` at open or mid-query — surfaces
//! as a [`CacheError`](crate::cache::error::CacheError) the owner treats as
//! fatal: it terminates, and a resummon rebuilds. There is no integrity-check
//! ladder and no retry-once machinery in the engine.
//!
//! # No per-operation write lock
//!
//! The owner-lifetime flock is `norn-owner`'s concern, not the engine's. Within
//! the engine there is a single writer thread by construction (the generation's
//! write connection, driven by the writer queue), so write paths take no
//! per-operation lock.

use std::collections::BTreeSet;

use camino::{Utf8Path, Utf8PathBuf};
use rusqlite::{params, Connection};

use crate::cache::error::CacheError;
use crate::cache::schema::apply_schema;

/// Handle to an opened cache: a rusqlite connection plus the resolved vault root
/// and config it was opened under.
///
/// `index_set` / `index_set_hash` are the resolved Wave-2 frontmatter-index
/// field set (ADR 0004); they drive what the `document_fields` EAV writer indexes
/// and what the query router may route to the index. The engine is always
/// authoritative — the db is created under a known index set at summon — so there
/// is no unconfigured-default open to suppress routing.
pub struct Cache {
    pub(crate) conn: Connection,
    pub(crate) vault_root: Utf8PathBuf,
    pub(crate) db_path: Utf8PathBuf,
    pub(crate) alias_field: Option<String>,
    pub(crate) files_ignore: Vec<String>,
    pub(crate) index_set: BTreeSet<String>,
    pub(crate) index_set_hash: String,
    /// Same-connection publication authority for reserved incremental jobs
    /// (ADR 0014). Defends internal concurrent publication only.
    pub(crate) increment_publication_epoch: u64,
}

impl Cache {
    /// Open (creating if absent) the cache db at `db_path` for `vault_root`,
    /// applying the schema. This is the primary, writable connection: the owner
    /// calls it at summon, then runs [`full_build`](Cache::full_build) as warm-up.
    pub fn create(
        db_path: impl AsRef<Utf8Path>,
        vault_root: impl AsRef<Utf8Path>,
        alias_field: Option<&str>,
        files_ignore: &[String],
        index_set: BTreeSet<String>,
        index_set_hash: &str,
    ) -> Result<Self, CacheError> {
        let db_path = db_path.as_ref().to_path_buf();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent.as_std_path()).map_err(|e| CacheError::Io {
                path: parent.to_owned(),
                source: e,
            })?;
        }
        let conn = Connection::open(db_path.as_std_path())?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
        apply_schema(&conn)?;
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', ?)",
            params![super::SCHEMA_VERSION.to_string()],
        )?;
        Ok(Self {
            conn,
            vault_root: vault_root.as_ref().to_path_buf(),
            db_path,
            alias_field: alias_field.map(str::to_string),
            files_ignore: files_ignore.to_vec(),
            index_set,
            index_set_hash: index_set_hash.to_string(),
            increment_publication_epoch: 0,
        })
    }

    /// Open a SECONDARY connection to an already-created cache db (the write
    /// companion, or a lazily-grown read connection). Does not apply schema — the
    /// primary already did — and never creates a fresh db.
    pub(crate) fn open_secondary(
        db_path: impl AsRef<Utf8Path>,
        vault_root: impl AsRef<Utf8Path>,
        alias_field: Option<&str>,
        files_ignore: &[String],
        index_set: BTreeSet<String>,
        index_set_hash: &str,
    ) -> Result<Self, CacheError> {
        let db_path = db_path.as_ref().to_path_buf();
        let conn = Connection::open(db_path.as_std_path())?;
        conn.execute_batch("PRAGMA busy_timeout=5000;")?;
        Ok(Self {
            conn,
            vault_root: vault_root.as_ref().to_path_buf(),
            db_path,
            alias_field: alias_field.map(str::to_string),
            files_ignore: files_ignore.to_vec(),
            index_set,
            index_set_hash: index_set_hash.to_string(),
            increment_publication_epoch: 0,
        })
    }

    /// Stamp `PRAGMA query_only = ON`, enforcing the read-only invariant on a
    /// pooled read connection (ADR 0013).
    pub(crate) fn set_query_only(&self) -> Result<(), CacheError> {
        self.conn.execute_batch("PRAGMA query_only=ON;")?;
        Ok(())
    }

    /// The db file this handle is attached to.
    pub fn db_path(&self) -> &Utf8Path {
        &self.db_path
    }

    /// The vault root this cache was opened for.
    pub fn vault_root(&self) -> &Utf8Path {
        &self.vault_root
    }

    /// Configured frontmatter field name used for alias parsing, if any.
    pub fn alias_field(&self) -> Option<&str> {
        self.alias_field.as_deref()
    }

    /// Crate-internal / test connection accessor. Not part of the stable API.
    #[doc(hidden)]
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Run a structured cache read against one SQLite snapshot.
    ///
    /// Callers open/refresh the cache before entering this closure, so freshness
    /// work never extends the snapshot lifetime. This transaction is read-only by
    /// convention and remains WAL-friendly for concurrent writers. Nested calls
    /// reuse the outer transaction, allowing snapshot-owning helpers to compose
    /// into a wider structured read phase without a nested `BEGIN`.
    pub fn read_snapshot<T>(
        &self,
        read: impl FnOnce(&Self) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        use anyhow::Context as _;

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

    /// Every distinct top-level frontmatter key observed across the vault's
    /// documents. Drives the dynamic-field-predicate universe (ADR 0010): a
    /// `--key value` guess is only accepted when `key` is a field this vault
    /// actually carries. Uses SQLite's JSON1 `json_each` over the cached
    /// `frontmatter_json`, so no filesystem re-parse is needed.
    pub fn observed_field_names(&self) -> Result<BTreeSet<String>, CacheError> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT je.key \
             FROM documents, json_each(documents.frontmatter_json) je \
             WHERE documents.frontmatter_json IS NOT NULL \
               AND json_valid(documents.frontmatter_json) \
               AND json_type(documents.frontmatter_json) = 'object'",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut fields = BTreeSet::new();
        for row in rows {
            fields.insert(row?);
        }
        Ok(fields)
    }
}

/// Shared debug-only `env → Duration` override reader for the cache tuning
/// knobs. **Release builds always return `default`** — the env read is
/// `#[cfg(debug_assertions)]`, compiled out entirely. `accept_zero` lets the
/// increment-chunk budget accept `0` (one-file-per-chunk in tests) while other
/// call sites reject it.
#[cfg_attr(not(debug_assertions), allow(unused_variables))]
pub(crate) fn debug_env_duration_ms(
    var: &str,
    default: std::time::Duration,
    accept_zero: bool,
) -> std::time::Duration {
    #[cfg(debug_assertions)]
    if let Ok(raw) = std::env::var(var) {
        if let Ok(ms) = raw.parse::<u64>() {
            if accept_zero || ms > 0 {
                return std::time::Duration::from_millis(ms);
            }
        }
    }
    default
}

/// Sibling of [`debug_env_duration_ms`] for a `usize` knob (debug builds only).
#[cfg_attr(not(debug_assertions), allow(unused_variables))]
pub(crate) fn debug_env_usize(var: &str, default: usize) -> usize {
    #[cfg(debug_assertions)]
    if let Ok(raw) = std::env::var(var) {
        if let Ok(value) = raw.parse::<usize>() {
            return value;
        }
    }
    default
}

#[cfg(test)]
impl Cache {
    /// Test constructor: open a cache over `root` with a db under a hidden
    /// `.norn-test-cache/` subdir (deterministic per root, so a reopen in the
    /// same test finds the same db; hidden, so the graph walk skips it).
    pub(crate) fn open(root: impl AsRef<Utf8Path>) -> Result<Self, CacheError> {
        Self::open_with_index(root, None, &[], BTreeSet::new(), "")
    }

    /// Test constructor with an alias field.
    pub(crate) fn open_with_config(
        root: impl AsRef<Utf8Path>,
        alias_field: Option<&str>,
    ) -> Result<Self, CacheError> {
        Self::open_with_index(root, alias_field, &[], BTreeSet::new(), "")
    }

    /// Test constructor with the full authoritative index config.
    pub(crate) fn open_with_index(
        root: impl AsRef<Utf8Path>,
        alias_field: Option<&str>,
        files_ignore: &[String],
        index_set: BTreeSet<String>,
        index_set_hash: &str,
    ) -> Result<Self, CacheError> {
        let root = root.as_ref().to_path_buf();
        let db_path = root.join(".norn-test-cache").join("cache.db");
        Self::create(
            db_path,
            root,
            alias_field,
            files_ignore,
            index_set,
            index_set_hash,
        )
    }
}
