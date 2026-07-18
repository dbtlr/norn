//! The registry: register / unregister / list / lookup / reverse-lookup.
//!
//! Reads are lock-free (parse the file if present; absent = empty). Mutations
//! take an exclusive advisory lock on a sidecar `.lock` file around a
//! read-modify-write, then write via a temp file in the same directory plus an
//! atomic rename.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use fs2::FileExt;

use crate::error::ConfigError;
use crate::home::ConfigHome;
use crate::model::{self, ConfigFile, VaultEntry};

/// A registered vault, as returned by lookups and listings. This is the public
/// projection of a stored entry (unknown per-vault keys are preserved on disk
/// but not surfaced here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredVault {
    /// Registered name.
    pub name: String,
    /// Absolute, canonicalized vault root.
    pub root: PathBuf,
    /// Explicit config-location override, if one was registered.
    pub config: Option<PathBuf>,
    /// Explicit cache-location override, if one was registered.
    pub cache: Option<PathBuf>,
    /// Explicit logs-location override, if one was registered.
    pub logs: Option<PathBuf>,
}

impl RegisteredVault {
    fn from_entry(name: &str, entry: &VaultEntry) -> Self {
        Self {
            name: name.to_string(),
            root: entry.root.clone(),
            config: entry.config.clone(),
            cache: entry.cache.clone(),
            logs: entry.logs.clone(),
        }
    }
}

/// Optional path overrides supplied at registration. Stored verbatim — never
/// canonicalized (they may name locations that do not exist yet) and never
/// synthesized from the root.
#[derive(Debug, Default, Clone)]
pub struct VaultOverrides {
    pub config: Option<PathBuf>,
    pub cache: Option<PathBuf>,
    pub logs: Option<PathBuf>,
}

/// An in-place change set for [`Registry::set`], the sanctioned mutation path
/// for a registered entry — so agents and humans never hand-edit the config to
/// fill gaps.
///
/// Every field is tri-state, encoding leave-untouched vs. change:
///
/// - `root`: `None` leaves the root unchanged; `Some(path)` re-points it (the
///   new root is canonicalized, must be an existing directory, and is
///   re-checked against one-name-per-root, excluding this entry itself).
/// - `config` / `cache` / `logs`: an `Option<Option<PathBuf>>` where the outer
///   `Option` is touched-vs-untouched and the inner is set-vs-clear —
///   `None` leaves the override as-is, `Some(None)` clears it, and
///   `Some(Some(path))` stores `path` verbatim (never canonicalized).
#[derive(Debug, Default, Clone)]
pub struct VaultChanges {
    pub root: Option<PathBuf>,
    pub config: Option<Option<PathBuf>>,
    pub cache: Option<Option<PathBuf>>,
    pub logs: Option<Option<PathBuf>>,
}

/// Validate a vault name against `[a-z0-9][a-z0-9_-]*`: non-empty, lowercase,
/// no path separators.
pub fn validate_name(name: &str) -> Result<(), ConfigError> {
    let mut chars = name.chars();
    let valid = match chars.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {
            chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(ConfigError::InvalidName {
            name: name.to_string(),
        })
    }
}

/// The central config / vault registry, bound to a [`ConfigHome`].
#[derive(Debug, Clone)]
pub struct Registry {
    home: ConfigHome,
}

impl Registry {
    /// Bind a registry to a config home.
    pub fn new(home: ConfigHome) -> Self {
        Self { home }
    }

    /// The config home this registry reads and writes.
    pub fn home(&self) -> &ConfigHome {
        &self.home
    }

    /// All registered vaults, deterministically ordered by name.
    pub fn list(&self) -> Result<Vec<RegisteredVault>, ConfigError> {
        let config = model::read(&self.home.config_path())?;
        Ok(config
            .vaults
            .iter()
            .map(|(name, entry)| RegisteredVault::from_entry(name, entry))
            .collect())
    }

    /// Look up a vault by exact name.
    pub fn lookup(&self, name: &str) -> Result<Option<RegisteredVault>, ConfigError> {
        let config = model::read(&self.home.config_path())?;
        Ok(config
            .vaults
            .get(name)
            .map(|entry| RegisteredVault::from_entry(name, entry)))
    }

    /// Find the registered vault whose root is an ancestor of `path` (or equals
    /// it). When roots nest, the nearest (deepest) root wins. Paths are
    /// canonicalized before comparing. Returns `None` when nothing matches.
    ///
    /// This is a pure path-ancestry query: it does not check that the matched
    /// root still exists — the resolver applies the fail-loud stale check.
    pub fn reverse_lookup(&self, path: &Path) -> Result<Option<RegisteredVault>, ConfigError> {
        let config = model::read(&self.home.config_path())?;
        let needle = canonicalize_best_effort(path);
        let mut best: Option<(usize, RegisteredVault)> = None;
        for (name, entry) in &config.vaults {
            let root = canonicalize_best_effort(&entry.root);
            if needle.starts_with(&root) {
                let depth = root.components().count();
                if best.as_ref().is_none_or(|(d, _)| depth > *d) {
                    best = Some((depth, RegisteredVault::from_entry(name, entry)));
                }
            }
        }
        Ok(best.map(|(_, vault)| vault))
    }

    /// Register a vault. Canonicalizes `root` (which must be an existing
    /// directory), rejects a duplicate name, and rejects a root already
    /// registered under another name (one name per root).
    pub fn register(
        &self,
        name: &str,
        root: &Path,
        overrides: VaultOverrides,
    ) -> Result<RegisteredVault, ConfigError> {
        validate_name(name)?;
        let canon_root = root
            .canonicalize()
            .map_err(|source| ConfigError::io("failed to canonicalize vault root", root, source))?;
        if !canon_root.is_dir() {
            return Err(ConfigError::RootNotDirectory { root: canon_root });
        }

        self.with_lock(|config| {
            if config.vaults.contains_key(name) {
                return Err(ConfigError::DuplicateName {
                    name: name.to_string(),
                });
            }
            for (existing_name, entry) in &config.vaults {
                if canonicalize_best_effort(&entry.root) == canon_root {
                    return Err(ConfigError::RootAlreadyRegistered {
                        root: canon_root.clone(),
                        existing: existing_name.clone(),
                    });
                }
            }
            let entry = VaultEntry {
                root: canon_root.clone(),
                config: overrides.config.clone(),
                cache: overrides.cache.clone(),
                logs: overrides.logs.clone(),
                extra: BTreeMap::new(),
            };
            let vault = RegisteredVault::from_entry(name, &entry);
            config.vaults.insert(name.to_string(), entry);
            Ok(vault)
        })
    }

    /// Mutate a registered entry in place under the same read-modify-write lock
    /// as [`Registry::register`]. `UnknownName` if the name is not registered.
    /// A changed `root` is canonicalized, must be an existing directory, and is
    /// re-checked against one-name-per-root (excluding this entry itself);
    /// overrides are stored verbatim (set) or dropped (clear). Unknown per-vault
    /// and top-level keys survive untouched.
    pub fn set(&self, name: &str, changes: VaultChanges) -> Result<RegisteredVault, ConfigError> {
        // Canonicalize a changed root before taking the lock — it touches the
        // filesystem and must not depend on ambient cwd, exactly as register.
        let new_root = match &changes.root {
            Some(root) => {
                let canon = root.canonicalize().map_err(|source| {
                    ConfigError::io("failed to canonicalize vault root", root, source)
                })?;
                if !canon.is_dir() {
                    return Err(ConfigError::RootNotDirectory { root: canon });
                }
                Some(canon)
            }
            None => None,
        };

        self.with_lock(|config| {
            if !config.vaults.contains_key(name) {
                return Err(ConfigError::UnknownName {
                    name: name.to_string(),
                });
            }
            // One-name-per-root, excluding the entry being edited.
            if let Some(canon) = &new_root {
                for (existing_name, entry) in &config.vaults {
                    if existing_name != name && canonicalize_best_effort(&entry.root) == *canon {
                        return Err(ConfigError::RootAlreadyRegistered {
                            root: canon.clone(),
                            existing: existing_name.clone(),
                        });
                    }
                }
            }
            // Mutate only the requested fields; `extra` (unknown keys) is never
            // touched, so future keys survive.
            let entry = config.vaults.get_mut(name).expect("presence checked above");
            if let Some(canon) = new_root {
                entry.root = canon;
            }
            if let Some(config_override) = changes.config {
                entry.config = config_override;
            }
            if let Some(cache_override) = changes.cache {
                entry.cache = cache_override;
            }
            if let Some(logs_override) = changes.logs {
                entry.logs = logs_override;
            }
            Ok(RegisteredVault::from_entry(name, entry))
        })
    }

    /// Remove a vault by name. Errors if no such name is registered.
    pub fn unregister(&self, name: &str) -> Result<(), ConfigError> {
        self.with_lock(|config| {
            if config.vaults.remove(name).is_none() {
                return Err(ConfigError::UnknownName {
                    name: name.to_string(),
                });
            }
            Ok(())
        })
    }

    /// Take the exclusive advisory lock, read the config, run `mutate`, and —
    /// only if it succeeds — write the result back atomically. The lock is held
    /// for the whole read-modify-write and released when the file handle drops.
    fn with_lock<T>(
        &self,
        mutate: impl FnOnce(&mut ConfigFile) -> Result<T, ConfigError>,
    ) -> Result<T, ConfigError> {
        let dir = self.home.dir();
        fs::create_dir_all(dir)
            .map_err(|source| ConfigError::io("failed to create config directory", dir, source))?;

        let lock_path = self.home.lock_path();
        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|source| ConfigError::io("failed to open config lock", &lock_path, source))?;
        lock_file
            .lock_exclusive()
            .map_err(|source| ConfigError::io("failed to lock config", &lock_path, source))?;

        let result = (|| {
            let mut config = model::read(&self.home.config_path())?;
            let value = mutate(&mut config)?;
            self.write_atomic(&config)?;
            Ok(value)
        })();

        // Best-effort explicit unlock; dropping the handle also releases it.
        let _ = FileExt::unlock(&lock_file);
        result
    }

    /// Write the config via a temp file in the config directory plus an atomic
    /// rename. Caller holds the lock.
    fn write_atomic(&self, config: &ConfigFile) -> Result<(), ConfigError> {
        let dir = self.home.dir();
        let config_path = self.home.config_path();
        let text = model::to_toml(config)?;

        let mut tmp = tempfile::Builder::new()
            .prefix(".config.toml.")
            .suffix(".tmp")
            .tempfile_in(dir)
            .map_err(|source| ConfigError::io("failed to create temp config", dir, source))?;
        tmp.write_all(text.as_bytes())
            .map_err(|source| ConfigError::io("failed to write temp config", tmp.path(), source))?;
        tmp.flush()
            .map_err(|source| ConfigError::io("failed to flush temp config", tmp.path(), source))?;
        // Crash durability, not just atomicity: sync the file content before
        // the rename, then sync the parent directory so the rename itself
        // survives power loss (a rename alone can be lost with the old file
        // already unlinked from the directory's flushed state).
        tmp.as_file()
            .sync_all()
            .map_err(|source| ConfigError::io("failed to sync temp config", tmp.path(), source))?;
        tmp.persist(&config_path)
            .map_err(|err| ConfigError::io("failed to persist config", &config_path, err.error))?;
        #[cfg(unix)]
        {
            let dir_handle = fs::File::open(dir).map_err(|source| {
                ConfigError::io("failed to open config directory for sync", dir, source)
            })?;
            dir_handle.sync_all().map_err(|source| {
                ConfigError::io("failed to sync config directory", dir, source)
            })?;
        }
        Ok(())
    }
}

/// Canonicalize when possible, otherwise fall back to the path as given. Stored
/// roots are canonical at register time, so the fallback only matters for a
/// stale (removed) root, where the stored form is the best available.
fn canonicalize_best_effort(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry_in(dir: &Path) -> Registry {
        Registry::new(ConfigHome::new(dir.join("norn")))
    }

    #[test]
    fn name_validation_accepts_and_rejects() {
        for good in ["a", "0", "docs", "my-vault", "v1_2", "a0-_z"] {
            assert!(validate_name(good).is_ok(), "should accept {good:?}");
        }
        for bad in [
            "", "-docs", "_docs", "Docs", "my vault", "a/b", "café", "UP",
        ] {
            assert!(
                matches!(validate_name(bad), Err(ConfigError::InvalidName { .. })),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn register_list_lookup_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let vault_dir = tmp.path().join("vault");
        fs::create_dir_all(&vault_dir).unwrap();
        let reg = registry_in(tmp.path());

        let created = reg
            .register("docs", &vault_dir, VaultOverrides::default())
            .unwrap();
        assert_eq!(created.name, "docs");
        assert_eq!(created.root, vault_dir.canonicalize().unwrap());

        let listed = reg.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "docs");

        let found = reg.lookup("docs").unwrap().unwrap();
        assert_eq!(found.root, vault_dir.canonicalize().unwrap());
        assert!(reg.lookup("missing").unwrap().is_none());
    }

    #[test]
    fn list_is_sorted_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        for name in ["zeta", "alpha", "mid"] {
            let d = tmp.path().join(name);
            fs::create_dir_all(&d).unwrap();
            reg.register(name, &d, VaultOverrides::default()).unwrap();
        }
        let names: Vec<_> = reg.list().unwrap().into_iter().map(|v| v.name).collect();
        assert_eq!(names, ["alpha", "mid", "zeta"]);
    }

    #[test]
    fn duplicate_name_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        reg.register("docs", &a, VaultOverrides::default()).unwrap();
        let err = reg
            .register("docs", &b, VaultOverrides::default())
            .unwrap_err();
        assert!(matches!(err, ConfigError::DuplicateName { .. }));
    }

    #[test]
    fn root_already_registered_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let vault = tmp.path().join("vault");
        fs::create_dir_all(&vault).unwrap();
        reg.register("docs", &vault, VaultOverrides::default())
            .unwrap();
        let err = reg
            .register("notes", &vault, VaultOverrides::default())
            .unwrap_err();
        match err {
            ConfigError::RootAlreadyRegistered { existing, .. } => assert_eq!(existing, "docs"),
            other => panic!("expected RootAlreadyRegistered, got {other:?}"),
        }
    }

    #[test]
    fn register_non_directory_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let file = tmp.path().join("a-file");
        fs::write(&file, "x").unwrap();
        let err = reg
            .register("docs", &file, VaultOverrides::default())
            .unwrap_err();
        assert!(matches!(err, ConfigError::RootNotDirectory { .. }));
    }

    #[test]
    fn register_missing_root_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let missing = tmp.path().join("nope");
        let err = reg
            .register("docs", &missing, VaultOverrides::default())
            .unwrap_err();
        assert!(matches!(err, ConfigError::Io { .. }));
    }

    #[test]
    fn overrides_are_stored_verbatim() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let vault = tmp.path().join("vault");
        fs::create_dir_all(&vault).unwrap();
        let overrides = VaultOverrides {
            config: Some(PathBuf::from("/somewhere/config")),
            cache: Some(PathBuf::from("/somewhere/cache")),
            logs: None,
        };
        let created = reg.register("docs", &vault, overrides).unwrap();
        assert_eq!(created.config, Some(PathBuf::from("/somewhere/config")));
        assert_eq!(created.cache, Some(PathBuf::from("/somewhere/cache")));
        assert_eq!(created.logs, None);
        // Persisted and re-read identically.
        let found = reg.lookup("docs").unwrap().unwrap();
        assert_eq!(found.config, Some(PathBuf::from("/somewhere/config")));
    }

    #[test]
    fn unregister_removes_and_reports_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let vault = tmp.path().join("vault");
        fs::create_dir_all(&vault).unwrap();
        reg.register("docs", &vault, VaultOverrides::default())
            .unwrap();
        reg.unregister("docs").unwrap();
        assert!(reg.list().unwrap().is_empty());
        let err = reg.unregister("docs").unwrap_err();
        assert!(matches!(err, ConfigError::UnknownName { .. }));
    }

    #[test]
    fn unregister_preserves_unknown_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        // Seed a config file with an unknown top-level key and two vaults, one
        // carrying an unknown per-vault key.
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::create_dir_all(reg.home().dir()).unwrap();
        let seeded = format!(
            "schema_version = 9\n\n[vaults.keep]\nroot = {a:?}\nauth_token = \"t\"\n\n[vaults.drop]\nroot = {b:?}\n",
            a = a.canonicalize().unwrap(),
            b = b.canonicalize().unwrap(),
        );
        fs::write(reg.home().config_path(), seeded).unwrap();

        reg.unregister("drop").unwrap();

        let text = fs::read_to_string(reg.home().config_path()).unwrap();
        assert!(
            text.contains("schema_version = 9"),
            "top-level key lost: {text}"
        );
        assert!(text.contains("auth_token"), "per-vault key lost: {text}");
        assert!(!text.contains("drop"), "removed vault lingered: {text}");
    }

    #[test]
    fn mutation_preserves_unknown_toplevel_and_nested_tables() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::create_dir_all(reg.home().dir()).unwrap();
        // Unknown top-level TABLE plus an unknown nested table inside a vault
        // entry — the flatten/serialization-order pitfall: a serializer that
        // emits [auth] after [vaults.*] would reparent it under a vault table.
        let seeded = format!(
            "[auth]\ntoken = \"t\"\nscopes = [\"read\"]\n\n[vaults.keep]\nroot = {a:?}\n\n[vaults.keep.remote]\nurl = \"https://example.test\"\n",
            a = a.canonicalize().unwrap(),
        );
        fs::write(reg.home().config_path(), seeded).unwrap();

        reg.register("added", &b, VaultOverrides::default())
            .unwrap();
        reg.unregister("added").unwrap();

        let reparsed: toml::Value =
            toml::from_str(&fs::read_to_string(reg.home().config_path()).unwrap()).unwrap();
        assert_eq!(
            reparsed["auth"]["token"].as_str(),
            Some("t"),
            "top-level [auth] table lost or reparented: {reparsed:?}"
        );
        assert_eq!(reparsed["auth"]["scopes"].as_array().map(Vec::len), Some(1));
        assert_eq!(
            reparsed["vaults"]["keep"]["remote"]["url"].as_str(),
            Some("https://example.test"),
            "nested vault table lost: {reparsed:?}"
        );
        assert!(reparsed["vaults"].get("added").is_none());
    }

    #[test]
    fn set_stores_and_clears_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let vault = tmp.path().join("vault");
        fs::create_dir_all(&vault).unwrap();
        reg.register("docs", &vault, VaultOverrides::default())
            .unwrap();

        // Set cache + logs verbatim, leave config untouched.
        let updated = reg
            .set(
                "docs",
                VaultChanges {
                    cache: Some(Some(PathBuf::from("/somewhere/cache"))),
                    logs: Some(Some(PathBuf::from("/somewhere/logs"))),
                    ..VaultChanges::default()
                },
            )
            .unwrap();
        assert_eq!(updated.cache, Some(PathBuf::from("/somewhere/cache")));
        assert_eq!(updated.logs, Some(PathBuf::from("/somewhere/logs")));
        assert_eq!(updated.config, None);

        // Clear cache, leave logs untouched.
        let updated = reg
            .set(
                "docs",
                VaultChanges {
                    cache: Some(None),
                    ..VaultChanges::default()
                },
            )
            .unwrap();
        assert_eq!(updated.cache, None);
        assert_eq!(updated.logs, Some(PathBuf::from("/somewhere/logs")));

        // Persisted identically.
        let found = reg.lookup("docs").unwrap().unwrap();
        assert_eq!(found.cache, None);
        assert_eq!(found.logs, Some(PathBuf::from("/somewhere/logs")));
    }

    #[test]
    fn set_default_changes_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let vault = tmp.path().join("vault");
        fs::create_dir_all(&vault).unwrap();
        let overrides = VaultOverrides {
            config: Some(PathBuf::from("/c")),
            cache: None,
            logs: Some(PathBuf::from("/l")),
        };
        reg.register("docs", &vault, overrides).unwrap();

        let updated = reg.set("docs", VaultChanges::default()).unwrap();
        assert_eq!(updated.root, vault.canonicalize().unwrap());
        assert_eq!(updated.config, Some(PathBuf::from("/c")));
        assert_eq!(updated.cache, None);
        assert_eq!(updated.logs, Some(PathBuf::from("/l")));
    }

    #[test]
    fn set_repoints_root_canonicalized() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let old = tmp.path().join("old");
        let new = tmp.path().join("new");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&new).unwrap();
        reg.register("docs", &old, VaultOverrides::default())
            .unwrap();

        let updated = reg
            .set(
                "docs",
                VaultChanges {
                    root: Some(new.clone()),
                    ..VaultChanges::default()
                },
            )
            .unwrap();
        assert_eq!(updated.root, new.canonicalize().unwrap());
    }

    #[test]
    fn set_root_to_non_directory_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let vault = tmp.path().join("vault");
        fs::create_dir_all(&vault).unwrap();
        reg.register("docs", &vault, VaultOverrides::default())
            .unwrap();
        let file = tmp.path().join("a-file");
        fs::write(&file, "x").unwrap();
        let err = reg
            .set(
                "docs",
                VaultChanges {
                    root: Some(file),
                    ..VaultChanges::default()
                },
            )
            .unwrap_err();
        assert!(matches!(err, ConfigError::RootNotDirectory { .. }));
    }

    #[test]
    fn set_missing_root_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let vault = tmp.path().join("vault");
        fs::create_dir_all(&vault).unwrap();
        reg.register("docs", &vault, VaultOverrides::default())
            .unwrap();
        let err = reg
            .set(
                "docs",
                VaultChanges {
                    root: Some(tmp.path().join("nope")),
                    ..VaultChanges::default()
                },
            )
            .unwrap_err();
        assert!(matches!(err, ConfigError::Io { .. }));
    }

    #[test]
    fn set_root_to_own_current_root_is_allowed() {
        // Re-pointing to the same root must not trip one-name-per-root against
        // the entry itself.
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let vault = tmp.path().join("vault");
        fs::create_dir_all(&vault).unwrap();
        reg.register("docs", &vault, VaultOverrides::default())
            .unwrap();
        let updated = reg
            .set(
                "docs",
                VaultChanges {
                    root: Some(vault.clone()),
                    ..VaultChanges::default()
                },
            )
            .unwrap();
        assert_eq!(updated.root, vault.canonicalize().unwrap());
    }

    #[test]
    fn set_root_to_another_entrys_root_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        reg.register("docs", &a, VaultOverrides::default()).unwrap();
        reg.register("notes", &b, VaultOverrides::default())
            .unwrap();
        // Point docs at notes' root.
        let err = reg
            .set(
                "docs",
                VaultChanges {
                    root: Some(b),
                    ..VaultChanges::default()
                },
            )
            .unwrap_err();
        match err {
            ConfigError::RootAlreadyRegistered { existing, .. } => assert_eq!(existing, "notes"),
            other => panic!("expected RootAlreadyRegistered, got {other:?}"),
        }
    }

    #[test]
    fn set_unknown_name_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let err = reg
            .set(
                "ghost",
                VaultChanges {
                    cache: Some(None),
                    ..VaultChanges::default()
                },
            )
            .unwrap_err();
        assert!(matches!(err, ConfigError::UnknownName { .. }));
    }

    #[test]
    fn set_preserves_unknown_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let a = tmp.path().join("a");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(reg.home().dir()).unwrap();
        // Unknown top-level key plus an unknown per-vault key on the target.
        let seeded = format!(
            "schema_version = 9\n\n[vaults.docs]\nroot = {a:?}\nauth_token = \"t\"\n",
            a = a.canonicalize().unwrap(),
        );
        fs::write(reg.home().config_path(), seeded).unwrap();

        reg.set(
            "docs",
            VaultChanges {
                cache: Some(Some(PathBuf::from("/new/cache"))),
                ..VaultChanges::default()
            },
        )
        .unwrap();

        let text = fs::read_to_string(reg.home().config_path()).unwrap();
        assert!(
            text.contains("schema_version = 9"),
            "top-level key lost: {text}"
        );
        assert!(text.contains("auth_token"), "per-vault key lost: {text}");
        assert!(text.contains("/new/cache"), "override not written: {text}");
    }

    #[test]
    fn reverse_lookup_nearest_root_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let outer = tmp.path().join("outer");
        let inner = outer.join("inner");
        fs::create_dir_all(&inner).unwrap();
        reg.register("outer", &outer, VaultOverrides::default())
            .unwrap();
        reg.register("inner", &inner, VaultOverrides::default())
            .unwrap();

        let deep = inner.join("sub/dir");
        fs::create_dir_all(&deep).unwrap();
        let hit = reg.reverse_lookup(&deep).unwrap().unwrap();
        assert_eq!(hit.name, "inner");

        // A path under outer but not under inner resolves to outer.
        let shallow = outer.join("elsewhere");
        fs::create_dir_all(&shallow).unwrap();
        let hit = reg.reverse_lookup(&shallow).unwrap().unwrap();
        assert_eq!(hit.name, "outer");
    }

    #[test]
    fn reverse_lookup_no_match_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let vault = tmp.path().join("vault");
        fs::create_dir_all(&vault).unwrap();
        reg.register("docs", &vault, VaultOverrides::default())
            .unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        assert!(reg.reverse_lookup(&outside).unwrap().is_none());
    }

    #[test]
    fn concurrent_registration_is_serialized() {
        use std::sync::Arc;
        use std::thread;

        let tmp = tempfile::tempdir().unwrap();
        let reg = Arc::new(registry_in(tmp.path()));
        let count = 16;
        let mut dirs = Vec::new();
        for i in 0..count {
            let d = tmp.path().join(format!("v{i}"));
            fs::create_dir_all(&d).unwrap();
            dirs.push(d);
        }

        let handles: Vec<_> = (0..count)
            .map(|i| {
                let reg = Arc::clone(&reg);
                let dir = dirs[i].clone();
                thread::spawn(move || {
                    reg.register(&format!("v{i}"), &dir, VaultOverrides::default())
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap().unwrap();
        }

        // With no locking, concurrent read-modify-write would drop entries.
        let listed = reg.list().unwrap();
        assert_eq!(
            listed.len(),
            count,
            "lost writes under concurrency: {listed:?}"
        );
    }
}
