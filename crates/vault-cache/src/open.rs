//! Cache::open implementation + permissions enforcement + meta init.

use camino::Utf8Path;
use rusqlite::Connection;

use crate::error::CacheError;
use crate::identity::cache_dir_for;

impl crate::Cache {
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
    pub fn open(vault_root: &Utf8Path) -> Result<Self, CacheError> {
        Self::open_with_config(vault_root, None)
    }

    /// Open the cache for a vault, passing the configured `links.alias_field`
    /// value. When `alias_field` differs from the value stored in the
    /// `links_alias_field` meta row (including the disabled/empty case), the
    /// cache is silently rebuilt so resolved links stay consistent with
    /// current config.
    pub fn open_with_config(
        vault_root: &Utf8Path,
        alias_field: Option<&str>,
    ) -> Result<Self, CacheError> {
        let (canonical, cache_dir) = cache_dir_for(vault_root)?;

        // Ensure cache directory exists at 0700.
        create_dir_secure(&cache_dir)?;

        let db_path = cache_dir.join("cache.db");
        let alias_field_owned: Option<String> = alias_field.map(|s| s.to_string());

        loop {
            let action = inspect_existing_cache(&db_path, &canonical, alias_field)?;
            match action {
                InspectResult::Fresh => {
                    return open_fresh(&cache_dir, &db_path, &canonical, alias_field);
                }
                InspectResult::Reuse(conn) => {
                    return Ok(crate::Cache {
                        conn,
                        vault_root: canonical,
                        cache_dir,
                        alias_field: alias_field_owned,
                    });
                }
                InspectResult::RebuildNeeded(reason) => {
                    emit_rebuild_message(&reason);
                    // Delete and loop back through; next pass takes the Fresh branch.
                    if db_path.as_std_path().exists() {
                        std::fs::remove_file(db_path.as_std_path()).map_err(|e| {
                            CacheError::Io {
                                path: db_path.clone(),
                                source: e,
                            }
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
    if let Err(e) = conn.pragma_update(None, "journal_mode", "WAL") {
        return Ok(InspectResult::RebuildNeeded(RebuildReason::Corrupted(
            format!("could not set journal_mode: {e}"),
        )));
    }

    // PRAGMA integrity_check
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
    if found_version > crate::SCHEMA_VERSION {
        return Ok(InspectResult::HardError(CacheError::SchemaNewer {
            found: found_version,
            expected: crate::SCHEMA_VERSION,
        }));
    }
    if found_version < crate::SCHEMA_VERSION {
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
    alias_field: Option<&str>,
) -> Result<crate::Cache, CacheError> {
    let conn = Connection::open(db_path.as_std_path())?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    secure_file(db_path)?;
    crate::schema::apply_schema(&conn)?;
    init_meta(&conn, canonical_root, alias_field)?;
    Ok(crate::Cache {
        conn,
        vault_root: canonical_root.to_owned(),
        cache_dir: cache_dir.to_owned(),
        alias_field: alias_field.map(|s| s.to_string()),
    })
}

fn emit_rebuild_message(reason: &RebuildReason) {
    let msg = match reason {
        RebuildReason::Corrupted(detail) => format!("cache is corrupted ({detail}); rebuilding"),
        RebuildReason::SchemaOlder { found } => {
            format!(
                "cache schema is v{found}, expected v{}; rebuilding",
                crate::SCHEMA_VERSION
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
        rusqlite::params!["schema_version", crate::SCHEMA_VERSION.to_string()],
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
        let cache = crate::Cache::open(&root).unwrap();
        assert!(cache.cache_dir.exists());
        assert!(cache.cache_dir.join("cache.db").exists());
    }

    #[test]
    fn reopening_existing_cache_does_not_recreate() {
        let (_tmp, root) = make_vault();
        let cache1 = crate::Cache::open(&root).unwrap();
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

        let cache2 = crate::Cache::open(&root).unwrap();
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
        let cache = crate::Cache::open(&root).unwrap();
        let schema_version: u32 = cache
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get::<_, String>(0).map(|s| s.parse().unwrap()),
            )
            .unwrap();
        assert_eq!(schema_version, crate::SCHEMA_VERSION);

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
        let cache = crate::Cache::open(&root).unwrap();
        let metadata = std::fs::metadata(cache.cache_dir.as_std_path()).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "cache dir should be 0700, got {:o}", mode);
    }

    #[cfg(unix)]
    #[test]
    fn cache_db_file_has_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, root) = make_vault();
        let cache = crate::Cache::open(&root).unwrap();
        let db_path = cache.cache_dir.join("cache.db");
        let metadata = std::fs::metadata(db_path.as_std_path()).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "cache db should be 0600, got {:o}", mode);
    }

    #[test]
    fn open_after_schema_too_old_rebuilds_silently() {
        let (_tmp, root) = make_vault();
        let cache = crate::Cache::open(&root).unwrap();
        // Tamper: set schema_version to 0 (older than this binary).
        cache
            .conn
            .execute(
                "UPDATE meta SET value = '0' WHERE key = 'schema_version'",
                [],
            )
            .unwrap();
        drop(cache);

        let cache2 = crate::Cache::open(&root).unwrap();
        // Should have rebuilt — schema_version is now the current value.
        let v: String = cache2
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v.parse::<u32>().unwrap(), crate::SCHEMA_VERSION);
    }

    #[test]
    fn open_with_newer_schema_returns_hard_error() {
        let (_tmp, root) = make_vault();
        let cache = crate::Cache::open(&root).unwrap();
        cache
            .conn
            .execute(
                "UPDATE meta SET value = '999' WHERE key = 'schema_version'",
                [],
            )
            .unwrap();
        drop(cache);

        let result = crate::Cache::open(&root);
        match result {
            Err(crate::CacheError::SchemaNewer { found, expected }) => {
                assert_eq!(found, 999);
                assert_eq!(expected, crate::SCHEMA_VERSION);
            }
            Err(other) => panic!("expected SchemaNewer, got {:?}", other),
            Ok(_) => panic!("expected SchemaNewer, got Ok(Cache)"),
        }
    }

    #[test]
    fn open_with_identity_drift_rebuilds_silently() {
        let (_tmp, root) = make_vault();
        let cache = crate::Cache::open(&root).unwrap();
        cache
            .conn
            .execute(
                "UPDATE meta SET value = '/some/other/path' WHERE key = 'vault_root'",
                [],
            )
            .unwrap();
        drop(cache);

        let cache2 = crate::Cache::open(&root).unwrap();
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
        let cache = crate::Cache::open(&root).unwrap();
        let db_path = cache.cache_dir.join("cache.db");
        drop(cache);

        // Truncate the db file to corrupt it.
        std::fs::write(db_path.as_std_path(), b"corrupt").unwrap();

        let cache2 = crate::Cache::open(&root).unwrap();
        // Should have rebuilt cleanly; schema present again.
        let v: String = cache2
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v.parse::<u32>().unwrap(), crate::SCHEMA_VERSION);
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
        let mut cache = crate::Cache::open_with_config(&vault_root, None).unwrap();
        cache.rebuild(&vault_root).unwrap();
        drop(cache);

        // Reopen with alias_field = Some("aliases") — expect rebuild on open.
        let cache = crate::Cache::open_with_config(&vault_root, Some("aliases")).unwrap();
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

        let mut cache = crate::Cache::open_with_config(&vault_root, None).unwrap();
        cache.rebuild(&vault_root).unwrap();
        drop(cache);

        // None -> Some: rebuild expected. Verify meta.
        let cache = crate::Cache::open_with_config(&vault_root, Some("aliases")).unwrap();
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
        let cache = crate::Cache::open_with_config(&vault_root, None).unwrap();
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

        let mut cache = crate::Cache::open(&vault_root).unwrap();
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
