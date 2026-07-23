//! The resolution order — the single public resolver entry point.
//!
//! Precedence, highest first: explicit path → explicit name → repo binding →
//! `NORN_ROOT` env → cwd reverse lookup. Steps that resolve *through the
//! registry* (name, binding, reverse lookup) fail loud if the resolved root no
//! longer exists. A cwd matching nothing yields the unregistered outcome, not
//! an error — the caller decides what that means (ephemeral tier, ADR 0017).
//!
//! Inputs are supplied by value (cwd, the `NORN_ROOT` value, the caller's
//! `-C`/`--vault` optionals) so resolution is a pure function of its inputs and
//! testable without mutating the process environment. The CLI wiring that maps
//! flags and reads ambient env onto [`ResolveInput`] is a later task.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::ConfigError;
use crate::registry::{RegisteredVault, Registry};

/// Environment variable naming a vault root directly (a root path;
/// empty/whitespace ignored). Consulted after the repo binding.
pub const NORN_ROOT_ENV: &str = "NORN_ROOT";

/// The committable repo-binding filename, discovered by walking up from the cwd.
pub const BINDING_FILENAME: &str = ".norn.toml";

/// Everything the resolver needs, supplied by value.
#[derive(Debug, Clone)]
pub struct ResolveInput {
    /// An explicit vault path (the CLI's `-C`), highest precedence.
    pub explicit_path: Option<PathBuf>,
    /// An explicit registered vault name (the CLI's `--vault`).
    pub explicit_name: Option<String>,
    /// The working directory to resolve from. Never read ambiently.
    pub cwd: PathBuf,
    /// The raw `NORN_ROOT` value (as read by the caller), or `None`.
    pub norn_root_env: Option<String>,
}

impl ResolveInput {
    /// A bare input rooted at `cwd` with no explicit path/name and no env.
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            explicit_path: None,
            explicit_name: None,
            cwd: cwd.into(),
            norn_root_env: None,
        }
    }
}

/// How a vault root was resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedVia {
    /// An explicit `-C` path. Does not go through the registry.
    ExplicitPath,
    /// An explicit registered name.
    ExplicitName,
    /// A repo binding file, named here so the caller can report it.
    RepoBinding { file: PathBuf },
    /// The `NORN_ROOT` environment variable (a direct root path).
    NornRootEnv,
    /// The cwd sits inside a registered vault (reverse lookup).
    ReverseLookup,
    /// The cwd matches no registered vault — the ephemeral/unregistered
    /// outcome. Not an error.
    UnregisteredCwd,
}

/// A resolved vault: its root, the registered name when known, and how it was
/// resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The vault root. Canonical for registry-resolved vias (`ExplicitName`,
    /// `RepoBinding`, `ReverseLookup`); for the direct-path vias
    /// (`ExplicitPath`, `NornRootEnv`) it is grounded against the cwd but NOT
    /// canonicalized — those paths may not exist yet, so callers must not
    /// assume symlink-free, `..`-free form here.
    pub root: PathBuf,
    pub name: Option<String>,
    pub via: ResolvedVia,
}

impl Registry {
    /// Resolve a vault following the full precedence order. See the module
    /// docs for the contract.
    pub fn resolve(&self, input: &ResolveInput) -> Result<Resolved, ConfigError> {
        // Deterministic only over absolute inputs: a relative cwd would make
        // grounding, the binding walk, and reverse lookup depend on ambient
        // process state.
        if !input.cwd.is_absolute() {
            return Err(ConfigError::RelativeCwd {
                path: input.cwd.clone(),
            });
        }

        // 1. Explicit path — direct, never through the registry.
        if let Some(path) = &input.explicit_path {
            return Ok(Resolved {
                root: ground(&input.cwd, path),
                name: None,
                via: ResolvedVia::ExplicitPath,
            });
        }

        // 2. Explicit name — through the registry, fail loud on stale.
        if let Some(name) = &input.explicit_name {
            let vault = self
                .lookup(name)?
                .ok_or_else(|| ConfigError::UnknownName { name: name.clone() })?;
            ensure_live(&vault)?;
            return Ok(resolved_from(vault, ResolvedVia::ExplicitName));
        }

        // 3. Repo binding — walk up for `.norn.toml`, resolve its name.
        if let Some((file, name)) = find_binding(&input.cwd)? {
            let vault = self
                .lookup(&name)?
                .ok_or(ConfigError::BindingUnregistered {
                    file: file.clone(),
                    name,
                })?;
            ensure_live(&vault)?;
            return Ok(resolved_from(vault, ResolvedVia::RepoBinding { file }));
        }

        // 4. NORN_ROOT — a direct root path.
        if let Some(raw) = input
            .norn_root_env
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Ok(Resolved {
                root: ground(&input.cwd, Path::new(raw)),
                name: None,
                via: ResolvedVia::NornRootEnv,
            });
        }

        // 5. cwd reverse lookup — through the registry, fail loud on stale.
        if let Some(vault) = self.reverse_lookup(&input.cwd)? {
            ensure_live(&vault)?;
            return Ok(resolved_from(vault, ResolvedVia::ReverseLookup));
        }

        // 6. Unregistered cwd — the ephemeral outcome, not an error.
        Ok(Resolved {
            root: input
                .cwd
                .canonicalize()
                .unwrap_or_else(|_| input.cwd.clone()),
            name: None,
            via: ResolvedVia::UnregisteredCwd,
        })
    }
}

fn resolved_from(vault: RegisteredVault, via: ResolvedVia) -> Resolved {
    Resolved {
        root: vault.root,
        name: Some(vault.name),
        via,
    }
}

/// A registry-resolved root must still be an existing directory, else fail loud.
fn ensure_live(vault: &RegisteredVault) -> Result<(), ConfigError> {
    if vault.root.is_dir() {
        Ok(())
    } else {
        Err(ConfigError::StaleEntry {
            name: vault.name.clone(),
            path: vault.root.clone(),
        })
    }
}

/// Absolute paths pass through; relative ones are grounded against `cwd`.
fn ground(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// The subset of a repo binding this crate models. Unknown keys are tolerated.
#[derive(Debug, Deserialize)]
struct Binding {
    vault: Option<String>,
    #[serde(flatten)]
    #[allow(dead_code)]
    extra: BTreeMap<String, toml::Value>,
}

/// Walk up from `cwd` looking for a `.norn.toml`. The nearest (deepest) one
/// wins. Returns its path and the bound vault name.
fn find_binding(cwd: &Path) -> Result<Option<(PathBuf, String)>, ConfigError> {
    for dir in cwd.ancestors() {
        let file = dir.join(BINDING_FILENAME);
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(ConfigError::io(
                    "failed to read repo binding",
                    &file,
                    source,
                ))
            }
        };
        let binding: Binding =
            toml::from_str(&text).map_err(|source| ConfigError::BindingParse {
                file: file.clone(),
                source,
            })?;
        let name = binding
            .vault
            .ok_or(ConfigError::BindingMissingVault { file: file.clone() })?;
        return Ok(Some((file, name)));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::ConfigHome;
    use crate::registry::VaultOverrides;
    use std::fs;

    fn registry_in(dir: &Path) -> Registry {
        Registry::new(ConfigHome::new(dir.join("norn")))
    }

    #[test]
    fn relative_cwd_is_rejected_at_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let input = ResolveInput::new(PathBuf::from("relative/cwd"));
        let err = reg.resolve(&input).unwrap_err();
        assert!(matches!(err, ConfigError::RelativeCwd { .. }));
    }

    #[test]
    fn explicit_path_wins_over_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let vault = tmp.path().join("vault");
        fs::create_dir_all(&vault).unwrap();
        reg.register("docs", &vault, VaultOverrides::default())
            .unwrap();

        // cwd is inside the registered vault AND a name is given AND NORN_ROOT
        // is set — explicit path still wins.
        let explicit = tmp.path().join("elsewhere");
        fs::create_dir_all(&explicit).unwrap();
        let input = ResolveInput {
            explicit_path: Some(explicit.clone()),
            explicit_name: Some("docs".into()),
            cwd: vault.clone(),
            norn_root_env: Some("/somewhere".into()),
        };
        let resolved = reg.resolve(&input).unwrap();
        assert_eq!(resolved.via, ResolvedVia::ExplicitPath);
        assert_eq!(resolved.root, explicit);
    }

    #[test]
    fn explicit_name_beats_binding_env_and_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let vault = tmp.path().join("vault");
        fs::create_dir_all(&vault).unwrap();
        reg.register("docs", &vault, VaultOverrides::default())
            .unwrap();

        // Put a binding in the cwd too; name should still win.
        let cwd = tmp.path().join("work");
        fs::create_dir_all(&cwd).unwrap();
        fs::write(cwd.join(BINDING_FILENAME), "vault = \"docs\"\n").unwrap();

        let input = ResolveInput {
            explicit_path: None,
            explicit_name: Some("docs".into()),
            cwd,
            norn_root_env: Some("/somewhere".into()),
        };
        let resolved = reg.resolve(&input).unwrap();
        assert_eq!(resolved.via, ResolvedVia::ExplicitName);
        assert_eq!(resolved.name.as_deref(), Some("docs"));
        assert_eq!(resolved.root, vault.canonicalize().unwrap());
    }

    #[test]
    fn unknown_explicit_name_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let input = ResolveInput {
            explicit_name: Some("ghost".into()),
            ..ResolveInput::new(tmp.path())
        };
        assert!(matches!(
            reg.resolve(&input).unwrap_err(),
            ConfigError::UnknownName { .. }
        ));
    }

    #[test]
    fn binding_beats_env_and_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let vault = tmp.path().join("vault");
        fs::create_dir_all(&vault).unwrap();
        reg.register("docs", &vault, VaultOverrides::default())
            .unwrap();

        // Binding lives above the cwd; walk-up must find it.
        let repo = tmp.path().join("repo");
        let deep = repo.join("a/b");
        fs::create_dir_all(&deep).unwrap();
        fs::write(repo.join(BINDING_FILENAME), "vault = \"docs\"\nother = 1\n").unwrap();

        let input = ResolveInput {
            cwd: deep,
            norn_root_env: Some("/env/root".into()),
            ..ResolveInput::new(tmp.path())
        };
        let resolved = reg.resolve(&input).unwrap();
        assert_eq!(
            resolved.via,
            ResolvedVia::RepoBinding {
                file: repo.join(BINDING_FILENAME)
            }
        );
        assert_eq!(resolved.name.as_deref(), Some("docs"));
    }

    #[test]
    fn binding_to_unregistered_vault_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let cwd = tmp.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        fs::write(cwd.join(BINDING_FILENAME), "vault = \"ghost\"\n").unwrap();
        let input = ResolveInput::new(cwd);
        match reg.resolve(&input).unwrap_err() {
            ConfigError::BindingUnregistered { name, .. } => assert_eq!(name, "ghost"),
            other => panic!("expected BindingUnregistered, got {other:?}"),
        }
    }

    #[test]
    fn binding_missing_vault_key_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let cwd = tmp.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        fs::write(cwd.join(BINDING_FILENAME), "other = 1\n").unwrap();
        assert!(matches!(
            reg.resolve(&ResolveInput::new(cwd)).unwrap_err(),
            ConfigError::BindingMissingVault { .. }
        ));
    }

    #[test]
    fn binding_parse_error_names_file() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let cwd = tmp.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        fs::write(cwd.join(BINDING_FILENAME), "this = = not toml\n").unwrap();
        assert!(matches!(
            reg.resolve(&ResolveInput::new(cwd)).unwrap_err(),
            ConfigError::BindingParse { .. }
        ));
    }

    #[test]
    fn norn_root_beats_cwd_reverse_lookup() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let vault = tmp.path().join("vault");
        fs::create_dir_all(&vault).unwrap();
        reg.register("docs", &vault, VaultOverrides::default())
            .unwrap();

        // cwd is inside the registered vault, but NORN_ROOT points elsewhere.
        let input = ResolveInput {
            cwd: vault.clone(),
            norn_root_env: Some("/env/root".into()),
            ..ResolveInput::new(vault.clone())
        };
        let resolved = reg.resolve(&input).unwrap();
        assert_eq!(resolved.via, ResolvedVia::NornRootEnv);
        assert_eq!(resolved.root, PathBuf::from("/env/root"));
    }

    #[test]
    fn empty_norn_root_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let vault = tmp.path().join("vault");
        fs::create_dir_all(&vault).unwrap();
        reg.register("docs", &vault, VaultOverrides::default())
            .unwrap();
        let input = ResolveInput {
            cwd: vault.clone(),
            norn_root_env: Some("   ".into()),
            ..ResolveInput::new(vault.clone())
        };
        let resolved = reg.resolve(&input).unwrap();
        assert_eq!(resolved.via, ResolvedVia::ReverseLookup);
    }

    #[test]
    fn cwd_reverse_lookup_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let vault = tmp.path().join("vault");
        let deep = vault.join("a/b");
        fs::create_dir_all(&deep).unwrap();
        reg.register("docs", &vault, VaultOverrides::default())
            .unwrap();

        let resolved = reg.resolve(&ResolveInput::new(deep)).unwrap();
        assert_eq!(resolved.via, ResolvedVia::ReverseLookup);
        assert_eq!(resolved.name.as_deref(), Some("docs"));
        assert_eq!(resolved.root, vault.canonicalize().unwrap());
    }

    #[test]
    fn unregistered_cwd_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let elsewhere = tmp.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        let resolved = reg.resolve(&ResolveInput::new(elsewhere.clone())).unwrap();
        assert_eq!(resolved.via, ResolvedVia::UnregisteredCwd);
        assert_eq!(resolved.name, None);
        assert_eq!(resolved.root, elsewhere.canonicalize().unwrap());
    }

    #[test]
    fn stale_entry_fails_loud_via_name() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let vault = tmp.path().join("vault");
        fs::create_dir_all(&vault).unwrap();
        reg.register("docs", &vault, VaultOverrides::default())
            .unwrap();
        fs::remove_dir_all(&vault).unwrap();

        let input = ResolveInput {
            explicit_name: Some("docs".into()),
            ..ResolveInput::new(tmp.path())
        };
        match reg.resolve(&input).unwrap_err() {
            ConfigError::StaleEntry { name, .. } => assert_eq!(name, "docs"),
            other => panic!("expected StaleEntry, got {other:?}"),
        }
    }

    #[test]
    fn stale_entry_fails_loud_via_binding() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let vault = tmp.path().join("vault");
        fs::create_dir_all(&vault).unwrap();
        reg.register("docs", &vault, VaultOverrides::default())
            .unwrap();
        fs::remove_dir_all(&vault).unwrap();

        let cwd = tmp.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        fs::write(cwd.join(BINDING_FILENAME), "vault = \"docs\"\n").unwrap();
        assert!(matches!(
            reg.resolve(&ResolveInput::new(cwd)).unwrap_err(),
            ConfigError::StaleEntry { .. }
        ));
    }
}
