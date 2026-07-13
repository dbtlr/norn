//! Cache::open implementation + permissions enforcement + meta init.

use std::collections::BTreeSet;

use camino::Utf8Path;
use rusqlite::Connection;

use crate::cache::error::CacheError;
use crate::cache::identity::cache_dir_for;

/// Lock-wait applied to every cache connection immediately after open.
///
/// A fresh open runs schema DDL and an inspecting open runs the
/// `journal_mode` / `integrity_check` pragmas — both take brief write locks
/// on the SQLite file. When two threads or processes open the same cache at
/// once (two concurrent `norn` invocations, or the `two_simultaneous_rebuilds`
/// concurrency test's two rebuild threads), SQLite's default zero lock-wait
/// makes the loser return `SQLITE_BUSY` immediately rather than waiting. A 5s
/// busy_timeout lets SQLite's own concurrency control absorb these brief
/// collisions, matching the 5s advisory flock that `rebuild` already holds.
/// This is deliberately cheaper than moving the schema DDL behind the advisory
/// lock, which would change `open`'s blocking semantics for every caller.
const CACHE_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(5000);

impl crate::cache::Cache {
    /// Open the cache for a vault. Creates the cache directory and database
    /// if missing; inspects an existing cache file and either reuses it,
    /// rebuilds it (corruption / older schema / identity drift), or hard-errors
    /// (schema newer than this binary supports).
    ///
    /// Thin wrapper around [`Cache::open_with_config`] that passes
    /// `alias_field = None`. Test and bootstrap call sites that don't have
    /// access to a loaded config should use this; production call sites with
    /// `LoadedConfig` in scope should use `open_with_config` so cached
    /// resolved-link state stays consistent with the operator's config.
    ///
    /// **Non-authoritative.** This opener has no real config knowledge — it
    /// never knows whether the operator's declared index set is genuinely
    /// empty or just unavailable here. It therefore never re-shreds
    /// `document_fields` and never stamps the `index_set_hash` meta row;
    /// an existing index is left exactly as-is for a later authoritative
    /// open to reconcile. Every production path that can write documents
    /// must open via `open_with_index`.
    pub fn open(vault_root: &Utf8Path) -> Result<Self, CacheError> {
        Self::open_with_config(vault_root, None)
    }

    /// Open the cache for a vault, passing the configured `links.alias_field`
    /// value. When `alias_field` differs from the value stored in the
    /// `links_alias_field` meta row (including the disabled/empty case), the
    /// cache is silently rebuilt so resolved links stay consistent with
    /// current config.
    ///
    /// Resolves the same (empty) index set `open_with_index` would compute
    /// for an unconfigured vault, but — unlike `open_with_index` — does NOT
    /// treat it as authoritative. See the non-authoritative note on
    /// [`Cache::open`]: call sites with a `LoadedConfig` in scope must use
    /// `open_with_index` directly so the `document_fields` EAV table stays
    /// consistent with the operator's current validate-rule / index.auto
    /// configuration.
    pub fn open_with_config(
        vault_root: &Utf8Path,
        alias_field: Option<&str>,
    ) -> Result<Self, CacheError> {
        let (index_set, index_set_hash) =
            crate::standards::resolved_index_set(&crate::standards::VaultConfig::default());
        open_impl(
            vault_root,
            alias_field,
            &[],
            &index_set,
            &index_set_hash,
            /* authoritative */ false,
        )
    }

    /// Open the cache for a vault, additionally passing the resolved Wave-2
    /// frontmatter-index field set (see
    /// `crate::standards::index_policy::resolved_index_set`). Production call
    /// sites with a `LoadedConfig` in scope should prefer this over
    /// `open_with_config` so the `document_fields` derived table stays
    /// consistent with the operator's current configuration.
    ///
    /// **Authoritative.** `index_set`/`index_set_hash` here are taken as
    /// ground truth: when an existing, otherwise-reusable cache's stored
    /// `index_set_hash` meta row disagrees (including "missing", for caches
    /// that predate this column), `document_fields` is silently re-shredded
    /// from the cached `frontmatter_json` column — no filesystem re-parse, no
    /// user-facing output — and the meta row is stamped with the new hash.
    pub fn open_with_index(
        vault_root: &Utf8Path,
        alias_field: Option<&str>,
        files_ignore: &[String],
        index_set: &BTreeSet<String>,
        index_set_hash: &str,
    ) -> Result<Self, CacheError> {
        open_impl(
            vault_root,
            alias_field,
            files_ignore,
            index_set,
            index_set_hash,
            /* authoritative */ true,
        )
    }

    /// Open a SECOND connection to a cache database that a primary
    /// [`open_with_index`](Cache::open_with_index) call has JUST opened and
    /// verified — skipping the `PRAGMA integrity_check` (and the schema /
    /// identity / alias-field inspection) the primary open already paid.
    ///
    /// # Why skipping verification is sound
    ///
    /// The caller opens this companion microseconds after the primary
    /// connection, against the SAME `cache.db` path, while holding the warm
    /// generation's sentinel `File` open on the primary inode. The held fd pins
    /// that inode from being freed — but an fd does NOT pin the PATH from being
    /// replaced, so a racing `cache clear` could still unlink+recreate `cache.db`
    /// and bind this by-path open to a DIFFERENT inode. The caller closes that
    /// hole itself: after this open, `open_generation` (in `crate::mcp::context`)
    /// stats the path and fails the generation open if the companion's `(dev,
    /// ino)` differs from the sentinel-captured identity, so a split-brain
    /// generation never escapes; a swap after that check is caught by the next
    /// request's ground-shift check. The primary `open_with_index` already ran
    /// `PRAGMA integrity_check`, reconciled schema version / vault identity /
    /// alias-field drift, and rebuilt if needed — so the bytes this companion
    /// attaches to are known-good under the held sentinel. Re-running the
    /// O(db-size) integrity check on the same file would verify nothing new.
    /// The companion still applies the operational pragmas every connection
    /// needs (WAL journal mode, busy timeout, foreign keys) — those are
    /// per-connection, not per-file.
    ///
    /// Used only by the warm daemon's per-generation WRITE connection
    /// (NRN-252): the request-facing connection stays read-only and this
    /// companion is the one the writer-queue freshness refresh writes through.
    /// Authoritative, matching the primary open, so a deferred first-touch
    /// `index_incremental` → `rebuild` stamps `index_set_hash` consistently.
    pub(crate) fn open_companion_verified(
        vault_root: &Utf8Path,
        alias_field: Option<&str>,
        files_ignore: &[String],
        index_set: &BTreeSet<String>,
        index_set_hash: &str,
    ) -> Result<Self, CacheError> {
        let (canonical, cache_dir) = cache_dir_for(vault_root)?;
        let db_path = cache_dir.join("cache.db");
        let conn = Connection::open(db_path.as_std_path())?;
        conn.busy_timeout(CACHE_BUSY_TIMEOUT)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(crate::cache::Cache {
            conn,
            vault_root: canonical,
            cache_dir,
            alias_field: alias_field.map(|s| s.to_string()),
            files_ignore: files_ignore.to_vec(),
            index_set: index_set.clone(),
            index_set_hash: index_set_hash.to_string(),
            index_authoritative: true,
        })
    }
}

/// The config-derived identity a `Cache` carries: the fields threaded from the
/// operator's config into every constructed `Cache`. Bundled so the internal
/// `open_impl`/`open_fresh` seam passes one reference instead of five positional
/// args.
struct CacheIdentity<'a> {
    alias_field: Option<&'a str>,
    files_ignore: &'a [String],
    index_set: &'a BTreeSet<String>,
    index_set_hash: &'a str,
    authoritative: bool,
}

/// Shared implementation behind `open`/`open_with_config` (non-authoritative)
/// and `open_with_index` (authoritative). `authoritative` gates whether a
/// reused cache is allowed to re-shred `document_fields` against
/// `index_set`/`index_set_hash` — see the doc comments on the public
/// constructors for the invariant this protects.
fn open_impl(
    vault_root: &Utf8Path,
    alias_field: Option<&str>,
    files_ignore: &[String],
    index_set: &BTreeSet<String>,
    index_set_hash: &str,
    authoritative: bool,
) -> Result<crate::cache::Cache, CacheError> {
    let (canonical, cache_dir) = cache_dir_for(vault_root)?;

    // Ensure cache directory exists at 0700.
    create_dir_secure(&cache_dir)?;

    let db_path = cache_dir.join("cache.db");
    let alias_field_owned: Option<String> = alias_field.map(|s| s.to_string());

    loop {
        let action = inspect_existing_cache(&db_path, &canonical, alias_field)?;
        match action {
            InspectResult::Fresh => {
                return open_fresh(
                    &cache_dir,
                    &db_path,
                    &canonical,
                    &CacheIdentity {
                        alias_field,
                        files_ignore,
                        index_set,
                        index_set_hash,
                        authoritative,
                    },
                );
            }
            InspectResult::Reuse(mut conn) => {
                if authoritative {
                    crate::cache::document_fields::reshred_if_needed(
                        &mut conn,
                        &cache_dir,
                        index_set,
                        index_set_hash,
                    )?;
                }
                return Ok(crate::cache::Cache {
                    conn,
                    vault_root: canonical,
                    cache_dir,
                    alias_field: alias_field_owned,
                    files_ignore: files_ignore.to_vec(),
                    index_set: index_set.clone(),
                    index_set_hash: index_set_hash.to_string(),
                    index_authoritative: authoritative,
                });
            }
            InspectResult::RebuildNeeded(reason) => {
                emit_rebuild_message(&reason);
                // Delete and loop back through; next pass takes the Fresh branch.
                if db_path.as_std_path().exists() {
                    std::fs::remove_file(db_path.as_std_path()).map_err(|e| CacheError::Io {
                        path: db_path.clone(),
                        source: e,
                    })?;
                }
                let wal = db_path.with_extension("db-wal");
                let shm = db_path.with_extension("db-shm");
                let _ = std::fs::remove_file(wal.as_std_path());
                let _ = std::fs::remove_file(shm.as_std_path());
            }
            InspectResult::HardError(err) => return Err(err),
        }
    }
}

#[derive(Debug)]
enum InspectResult {
    /// No cache file present; create from scratch.
    Fresh,
    /// Cache is valid and current; reuse the open connection.
    Reuse(Connection),
    /// Cache is recoverable by rebuild.
    RebuildNeeded(RebuildReason),
    /// Cache state cannot be safely interpreted; abort.
    HardError(CacheError),
}

#[derive(Debug)]
enum RebuildReason {
    Corrupted(String),
    SchemaOlder { found: u32 },
    IdentityDrift { cached: String, current: String },
    AliasFieldDrift { cached: String, current: String },
}

fn inspect_existing_cache(
    db_path: &Utf8Path,
    canonical_root: &Utf8Path,
    alias_field: Option<&str>,
) -> Result<InspectResult, CacheError> {
    if !db_path.as_std_path().exists() {
        return Ok(InspectResult::Fresh);
    }

    let conn = match Connection::open(db_path.as_std_path()) {
        Ok(c) => c,
        Err(e) => {
            return Ok(InspectResult::RebuildNeeded(RebuildReason::Corrupted(
                format!("could not open: {e}"),
            )));
        }
    };
    if let Err(e) = conn.busy_timeout(CACHE_BUSY_TIMEOUT) {
        return Ok(InspectResult::RebuildNeeded(RebuildReason::Corrupted(
            format!("could not set busy_timeout: {e}"),
        )));
    }
    if let Err(e) = conn.pragma_update(None, "journal_mode", "WAL") {
        return Ok(InspectResult::RebuildNeeded(RebuildReason::Corrupted(
            format!("could not set journal_mode: {e}"),
        )));
    }

    // PRAGMA integrity_check
    //
    // Acceptance-benchmark trace hook (NRN-83). When `NORN_TRACE_INTEGRITY_CHECK`
    // is set, emit one stable stderr marker per `integrity_check` execution. This
    // gives an out-of-process harness a deterministic, cross-process count of how
    // many times a code path actually pays the O(db-size) check: a live daemon
    // opens-once/verifies-once, so N routed reads share ONE marker, whereas N
    // direct invocations each reopen and produce N markers.
    //
    // Contract:
    // - Opt-in diagnostic, OFF by default: with the env var unset (every normal
    //   environment) this emits nothing and changes no behavior.
    // - When enabled, routed and direct stderr INTENTIONALLY diverge — revealing
    //   where the check runs is the hook's entire purpose, so the byte-identical
    //   routing guarantee does not hold under trace.
    // - Never enable it in an environment that asserts on norn's stderr (the
    //   routing byte-identity proofs, log-diffing pipelines).
    //
    // Deliberately available in RELEASE builds, unlike the debug-gated
    // `NORN_CACHE_LOCK_TIMEOUT_MS` (src/cache/lock.rs): that override CHANGES
    // timing behavior, so an inherited value in a production environment must
    // be impossible; this hook is pure observability — it alters no timing, no
    // outcome, no output when unset — and the `--release` acceptance benchmark
    // needs it in the optimized binary it measures.
    if std::env::var_os("NORN_TRACE_INTEGRITY_CHECK").is_some() {
        eprintln!("norn trace: integrity_check");
    }
    let integrity: Result<String, _> = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0));
    match integrity {
        Ok(s) if s == "ok" => {}
        Ok(s) => {
            return Ok(InspectResult::RebuildNeeded(RebuildReason::Corrupted(s)));
        }
        Err(e) => {
            return Ok(InspectResult::RebuildNeeded(RebuildReason::Corrupted(
                format!("integrity_check failed: {e}"),
            )));
        }
    }

    // Schema version check
    let sv: Result<String, _> = conn.query_row(
        "SELECT value FROM meta WHERE key = 'schema_version'",
        [],
        |r| r.get(0),
    );
    let found_version: u32 = match sv {
        Ok(s) => s.parse().unwrap_or(0),
        Err(_) => {
            return Ok(InspectResult::RebuildNeeded(RebuildReason::Corrupted(
                "missing schema_version meta row".to_string(),
            )));
        }
    };
    if found_version > crate::cache::SCHEMA_VERSION {
        return Ok(InspectResult::HardError(CacheError::SchemaNewer {
            found: found_version,
            expected: crate::cache::SCHEMA_VERSION,
        }));
    }
    if found_version < crate::cache::SCHEMA_VERSION {
        return Ok(InspectResult::RebuildNeeded(RebuildReason::SchemaOlder {
            found: found_version,
        }));
    }

    // Identity check
    let cached_root: Result<String, _> =
        conn.query_row("SELECT value FROM meta WHERE key = 'vault_root'", [], |r| {
            r.get(0)
        });
    match cached_root {
        Ok(s) if s == canonical_root.as_str() => {}
        Ok(s) => {
            return Ok(InspectResult::RebuildNeeded(RebuildReason::IdentityDrift {
                cached: s,
                current: canonical_root.as_str().to_string(),
            }));
        }
        Err(_) => {
            return Ok(InspectResult::RebuildNeeded(RebuildReason::Corrupted(
                "missing vault_root meta row".to_string(),
            )));
        }
    }

    // Alias-field check. The `links_alias_field` meta row is written on
    // every fresh open and rebuild as either the configured field name or
    // the empty string when the feature is disabled. Caches built before
    // this row existed return Err here; treat that the same as "empty" so
    // a None -> None reopen reuses the cache cleanly.
    let cached_alias: Result<String, _> = conn.query_row(
        "SELECT value FROM meta WHERE key = 'links_alias_field'",
        [],
        |r| r.get(0),
    );
    let cached_alias_str = cached_alias.unwrap_or_default();
    let current_alias_str = alias_field.unwrap_or("").to_string();
    if cached_alias_str != current_alias_str {
        return Ok(InspectResult::RebuildNeeded(
            RebuildReason::AliasFieldDrift {
                cached: cached_alias_str,
                current: current_alias_str,
            },
        ));
    }

    Ok(InspectResult::Reuse(conn))
}

fn open_fresh(
    cache_dir: &Utf8Path,
    db_path: &Utf8Path,
    canonical_root: &Utf8Path,
    identity: &CacheIdentity,
) -> Result<crate::cache::Cache, CacheError> {
    let conn = Connection::open(db_path.as_std_path())?;
    conn.busy_timeout(CACHE_BUSY_TIMEOUT)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    secure_file(db_path)?;
    crate::cache::schema::apply_schema(&conn)?;
    init_meta(&conn, canonical_root, identity.alias_field)?;
    Ok(crate::cache::Cache {
        conn,
        vault_root: canonical_root.to_owned(),
        cache_dir: cache_dir.to_owned(),
        alias_field: identity.alias_field.map(|s| s.to_string()),
        files_ignore: identity.files_ignore.to_vec(),
        index_set: identity.index_set.clone(),
        index_set_hash: identity.index_set_hash.to_string(),
        index_authoritative: identity.authoritative,
    })
}

fn emit_rebuild_message(reason: &RebuildReason) {
    let msg = match reason {
        RebuildReason::Corrupted(detail) => format!("cache is corrupted ({detail}); rebuilding"),
        RebuildReason::SchemaOlder { found } => {
            format!(
                "cache schema is v{found}, expected v{}; rebuilding",
                crate::cache::SCHEMA_VERSION
            )
        }
        RebuildReason::IdentityDrift { cached, current } => {
            format!("cache was built against {cached}, current vault is {current}; rebuilding")
        }
        RebuildReason::AliasFieldDrift { cached, current } => {
            let cached_disp = if cached.is_empty() {
                "<disabled>".to_string()
            } else {
                cached.clone()
            };
            let current_disp = if current.is_empty() {
                "<disabled>".to_string()
            } else {
                current.clone()
            };
            format!(
                "cache was built with links.alias_field = {cached_disp}, current config is {current_disp}; rebuilding"
            )
        }
    };
    eprintln!("vault: {msg}");
}

fn create_dir_secure(dir: &Utf8Path) -> Result<(), CacheError> {
    std::fs::create_dir_all(dir.as_std_path()).map_err(|e| CacheError::Io {
        path: dir.to_owned(),
        source: e,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(dir.as_std_path(), perms).map_err(|e| CacheError::Io {
            path: dir.to_owned(),
            source: e,
        })?;
    }
    Ok(())
}

fn secure_file(path: &Utf8Path) -> Result<(), CacheError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path.as_std_path(), perms).map_err(|e| CacheError::Io {
            path: path.to_owned(),
            source: e,
        })?;
    }
    let _ = path; // suppress unused on non-unix
    Ok(())
}

fn init_meta(
    conn: &Connection,
    canonical_root: &Utf8Path,
    alias_field: Option<&str>,
) -> Result<(), CacheError> {
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?, ?)",
        rusqlite::params!["schema_version", crate::cache::SCHEMA_VERSION.to_string()],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?, ?)",
        rusqlite::params!["vault_root", canonical_root.as_str()],
    )?;
    // Always present so drift-detection is a straight string comparison.
    // Empty string represents the alias feature being disabled.
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?, ?)",
        rusqlite::params!["links_alias_field", alias_field.unwrap_or("")],
    )?;
    let created_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string();
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?, ?)",
        rusqlite::params!["cache_created_ts", created_ts],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    fn make_vault() -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        // Minimal vault: empty dir is OK for open-flow testing.
        (tmp, root)
    }

    #[test]
    fn opening_a_fresh_vault_creates_cache_db() {
        let (_tmp, root) = make_vault();
        let cache = crate::cache::Cache::open(&root).unwrap();
        assert!(cache.cache_dir.exists());
        assert!(cache.cache_dir.join("cache.db").exists());
    }

    #[test]
    fn reopening_existing_cache_does_not_recreate() {
        let (_tmp, root) = make_vault();
        let cache1 = crate::cache::Cache::open(&root).unwrap();
        let path1 = cache1.cache_dir.join("cache.db");
        // Stamp the cache_created_ts so we can detect if init_meta runs again
        // on reopen (which would mean we recreated rather than reused).
        cache1
            .conn
            .execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('cache_created_ts', 'STAMP-DO-NOT-CHANGE')",
                [],
            )
            .unwrap();
        #[cfg(unix)]
        let ino1 = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(path1.as_std_path()).unwrap().ino()
        };
        drop(cache1);

        let cache2 = crate::cache::Cache::open(&root).unwrap();
        let path2 = cache2.cache_dir.join("cache.db");
        assert_eq!(path1, path2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let ino2 = std::fs::metadata(path2.as_std_path()).unwrap().ino();
            assert_eq!(ino1, ino2, "cache.db inode should not change on reopen");
        }
        // The stamp value should be preserved — meta init must NOT have re-run.
        let stamp: String = cache2
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'cache_created_ts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stamp, "STAMP-DO-NOT-CHANGE");
    }

    #[test]
    fn meta_rows_present_after_open() {
        let (_tmp, root) = make_vault();
        let cache = crate::cache::Cache::open(&root).unwrap();
        let schema_version: u32 = cache
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get::<_, String>(0).map(|s| s.parse().unwrap()),
            )
            .unwrap();
        assert_eq!(schema_version, crate::cache::SCHEMA_VERSION);

        let vault_root: String = cache
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'vault_root'", [], |r| {
                r.get(0)
            })
            .unwrap();
        // Should be the canonical path of the temp dir.
        assert!(vault_root.contains(root.file_name().unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn cache_directory_has_0700_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, root) = make_vault();
        let cache = crate::cache::Cache::open(&root).unwrap();
        let metadata = std::fs::metadata(cache.cache_dir.as_std_path()).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "cache dir should be 0700, got {:o}", mode);
    }

    #[cfg(unix)]
    #[test]
    fn cache_db_file_has_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, root) = make_vault();
        let cache = crate::cache::Cache::open(&root).unwrap();
        let db_path = cache.cache_dir.join("cache.db");
        let metadata = std::fs::metadata(db_path.as_std_path()).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "cache db should be 0600, got {:o}", mode);
    }

    #[test]
    fn open_after_schema_too_old_rebuilds_silently() {
        let (_tmp, root) = make_vault();
        let cache = crate::cache::Cache::open(&root).unwrap();
        // Tamper: set schema_version to 0 (older than this binary).
        cache
            .conn
            .execute(
                "UPDATE meta SET value = '0' WHERE key = 'schema_version'",
                [],
            )
            .unwrap();
        drop(cache);

        let cache2 = crate::cache::Cache::open(&root).unwrap();
        // Should have rebuilt — schema_version is now the current value.
        let v: String = cache2
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v.parse::<u32>().unwrap(), crate::cache::SCHEMA_VERSION);
    }

    #[test]
    fn open_with_newer_schema_returns_hard_error() {
        let (_tmp, root) = make_vault();
        let cache = crate::cache::Cache::open(&root).unwrap();
        cache
            .conn
            .execute(
                "UPDATE meta SET value = '999' WHERE key = 'schema_version'",
                [],
            )
            .unwrap();
        drop(cache);

        let result = crate::cache::Cache::open(&root);
        match result {
            Err(crate::cache::CacheError::SchemaNewer { found, expected }) => {
                assert_eq!(found, 999);
                assert_eq!(expected, crate::cache::SCHEMA_VERSION);
            }
            Err(other) => panic!("expected SchemaNewer, got {:?}", other),
            Ok(_) => panic!("expected SchemaNewer, got Ok(Cache)"),
        }
    }

    #[test]
    fn open_with_identity_drift_rebuilds_silently() {
        let (_tmp, root) = make_vault();
        let cache = crate::cache::Cache::open(&root).unwrap();
        cache
            .conn
            .execute(
                "UPDATE meta SET value = '/some/other/path' WHERE key = 'vault_root'",
                [],
            )
            .unwrap();
        drop(cache);

        let cache2 = crate::cache::Cache::open(&root).unwrap();
        let vr: String = cache2
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'vault_root'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(vr.contains(root.file_name().unwrap()));
    }

    #[test]
    fn open_after_corruption_rebuilds_silently() {
        let (_tmp, root) = make_vault();
        let cache = crate::cache::Cache::open(&root).unwrap();
        let db_path = cache.cache_dir.join("cache.db");
        drop(cache);

        // Truncate the db file to corrupt it.
        std::fs::write(db_path.as_std_path(), b"corrupt").unwrap();

        let cache2 = crate::cache::Cache::open(&root).unwrap();
        // Should have rebuilt cleanly; schema present again.
        let v: String = cache2
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v.parse::<u32>().unwrap(), crate::cache::SCHEMA_VERSION);
    }

    #[test]
    fn open_with_alias_field_drift_rebuilds_silently() {
        // 1. Build cache with alias_field = None
        // 2. Reopen with alias_field = Some("aliases") — expect a silent rebuild
        // 3. Verify the meta row `links_alias_field` now contains "aliases"
        let dir = tempfile::Builder::new()
            .prefix("vault-cache-alias-drift-")
            .tempdir()
            .unwrap();
        let base = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let vault_root = base.join("vault");
        std::fs::create_dir_all(&vault_root).unwrap();
        std::fs::write(vault_root.join("a.md"), "# A\n").unwrap();

        // Initial build: alias_field = None
        let mut cache = crate::cache::Cache::open_with_config(&vault_root, None).unwrap();
        cache.rebuild(&vault_root).unwrap();
        drop(cache);

        // Reopen with alias_field = Some("aliases") — expect rebuild on open.
        let cache = crate::cache::Cache::open_with_config(&vault_root, Some("aliases")).unwrap();
        let alias_meta: String = cache
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'links_alias_field'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(alias_meta, "aliases");
    }

    #[test]
    fn open_with_alias_field_disabled_then_enabled_then_disabled_triggers_two_rebuilds() {
        // Tests the full lifecycle: None -> Some -> None should each rebuild.
        let dir = tempfile::Builder::new()
            .prefix("vault-cache-alias-cycle-")
            .tempdir()
            .unwrap();
        let base = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let vault_root = base.join("vault");
        std::fs::create_dir_all(&vault_root).unwrap();
        std::fs::write(vault_root.join("a.md"), "# A\n").unwrap();

        let mut cache = crate::cache::Cache::open_with_config(&vault_root, None).unwrap();
        cache.rebuild(&vault_root).unwrap();
        drop(cache);

        // None -> Some: rebuild expected. Verify meta.
        let cache = crate::cache::Cache::open_with_config(&vault_root, Some("aliases")).unwrap();
        let v: String = cache
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'links_alias_field'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, "aliases");
        drop(cache);

        // Some -> None: rebuild expected. Verify meta now empty.
        let cache = crate::cache::Cache::open_with_config(&vault_root, None).unwrap();
        let v: String = cache
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'links_alias_field'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, "");
    }

    #[test]
    fn open_with_index_reshreds_document_fields_on_hash_mismatch() {
        let dir = tempfile::Builder::new()
            .prefix("vault-cache-reshred-mismatch-")
            .tempdir()
            .unwrap();
        let base = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let vault_root = base.join("vault");
        std::fs::create_dir_all(&vault_root).unwrap();
        std::fs::write(vault_root.join("a.md"), "---\nstatus: active\n---\n# A\n").unwrap();

        let set1: BTreeSet<String> = ["status".to_string()].into_iter().collect();
        let mut cache =
            crate::cache::Cache::open_with_index(&vault_root, None, &[], &set1, "hash-1").unwrap();
        cache.rebuild(&vault_root).unwrap();
        drop(cache);

        // Reopen with a different resolved index set / hash — should
        // silently re-shred document_fields from cached frontmatter_json.
        let set2: BTreeSet<String> = ["other".to_string()].into_iter().collect();
        let cache2 =
            crate::cache::Cache::open_with_index(&vault_root, None, &[], &set2, "hash-2").unwrap();

        let status_rows: i64 = cache2
            .conn
            .query_row(
                "SELECT COUNT(*) FROM document_fields WHERE key = 'status'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            status_rows, 0,
            "old field should have no rows after re-shred"
        );

        let other_rows: i64 = cache2
            .conn
            .query_row(
                "SELECT COUNT(*) FROM document_fields WHERE key = 'other'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            other_rows, 1,
            "newly-declared field should get a sentinel row after re-shred"
        );

        let stamped: String = cache2
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'index_set_hash'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stamped, "hash-2");
    }

    #[test]
    fn open_with_index_skips_reshred_when_hash_matches() {
        let dir = tempfile::Builder::new()
            .prefix("vault-cache-reshred-match-")
            .tempdir()
            .unwrap();
        let base = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let vault_root = base.join("vault");
        std::fs::create_dir_all(&vault_root).unwrap();
        std::fs::write(vault_root.join("a.md"), "---\nstatus: active\n---\n# A\n").unwrap();

        let set: BTreeSet<String> = ["status".to_string()].into_iter().collect();
        let mut cache =
            crate::cache::Cache::open_with_index(&vault_root, None, &[], &set, "hash-1").unwrap();
        cache.rebuild(&vault_root).unwrap();

        // Tamper a document_fields row directly; reopening with the SAME
        // hash must not re-shred, so the tamper should persist.
        cache
            .conn
            .execute(
                "UPDATE document_fields SET value = 'tampered' WHERE path = 'a.md' AND key = 'status'",
                [],
            )
            .unwrap();
        drop(cache);

        let cache2 =
            crate::cache::Cache::open_with_index(&vault_root, None, &[], &set, "hash-1").unwrap();
        let value: String = cache2
            .conn
            .query_row(
                "SELECT value FROM document_fields WHERE path = 'a.md' AND key = 'status'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(value, "tampered", "matching hash must skip re-shred");
    }

    #[test]
    fn non_authoritative_open_never_reshreds_or_stamps_index_set_hash() {
        // Regression for the live-reproduced CRITICAL bug: any opener
        // without real config knowledge (`Cache::open` / `open_with_config`,
        // e.g. `norn find --help`'s live-examples path) must NEVER treat its
        // unconfigured-default empty index set as authoritative — doing so
        // silently deleted every `document_fields` row and stamped the
        // empty-set hash on a cache that was actually built against a real
        // configured index set.
        let dir = tempfile::Builder::new()
            .prefix("vault-cache-nonauth-open-")
            .tempdir()
            .unwrap();
        let base = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let vault_root = base.join("vault");
        std::fs::create_dir_all(&vault_root).unwrap();
        std::fs::write(vault_root.join("a.md"), "---\nstatus: active\n---\n# A\n").unwrap();

        // Authoritative build against a real configured index set.
        let set: BTreeSet<String> = ["status".to_string()].into_iter().collect();
        let mut cache =
            crate::cache::Cache::open_with_index(&vault_root, None, &[], &set, "real-config-hash")
                .unwrap();
        cache.rebuild(&vault_root).unwrap();
        drop(cache);

        let before_rows: i64 = {
            let cache = crate::cache::Cache::open_with_index(
                &vault_root,
                None,
                &[],
                &set,
                "real-config-hash",
            )
            .unwrap();
            cache
                .conn
                .query_row("SELECT COUNT(*) FROM document_fields", [], |r| r.get(0))
                .unwrap()
        };
        assert!(
            before_rows > 0,
            "authoritative rebuild should have populated document_fields"
        );

        // A non-authoritative open (no config knowledge) must leave both the
        // rows and the stamped hash completely untouched.
        let non_auth = crate::cache::Cache::open(&vault_root).unwrap();
        let after_rows: i64 = non_auth
            .conn
            .query_row("SELECT COUNT(*) FROM document_fields", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            after_rows, before_rows,
            "non-authoritative open must not delete/re-shred document_fields rows"
        );
        let stamped: String = non_auth
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'index_set_hash'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            stamped, "real-config-hash",
            "non-authoritative open must not stamp its own (unconfigured-default) hash"
        );
    }

    #[test]
    fn open_legacy_call_preserves_pre_feature_behavior() {
        // Cache::open(vault_root) without _with_config must behave exactly like
        // open_with_config(vault_root, None) — preserves existing call sites.
        let dir = tempfile::Builder::new()
            .prefix("vault-cache-legacy-")
            .tempdir()
            .unwrap();
        let base = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let vault_root = base.join("vault");
        std::fs::create_dir_all(&vault_root).unwrap();
        std::fs::write(vault_root.join("a.md"), "# A\n").unwrap();

        let mut cache = crate::cache::Cache::open(&vault_root).unwrap();
        cache.rebuild(&vault_root).unwrap();

        let v: String = cache
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'links_alias_field'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, "");
    }
}
