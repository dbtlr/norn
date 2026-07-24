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
use crate::registry::{canonicalize_best_effort, RegisteredVault, Registry};

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

/// A resolved vault: its root, the registered name when known, how it was
/// resolved, and the matched registry entry when resolution went through the
/// registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The vault root. Canonical for every via: registry-resolved vias
    /// (`ExplicitName`, `RepoBinding`, `ReverseLookup`) already store a
    /// canonical root; the direct-path vias (`ExplicitPath`, `NornRootEnv`)
    /// are grounded against the cwd and then canonicalized by `ground`
    /// (NRN-415), and `UnregisteredCwd` canonicalizes its cwd directly. A root
    /// that does not exist yet cannot canonicalize, so it falls back to the
    /// grounded-but-uncanonicalized form in that case — callers must not
    /// assume symlink-free, `..`-free form when the root may not exist.
    pub root: PathBuf,
    pub name: Option<String>,
    pub via: ResolvedVia,
    /// The registry entry that produced this resolution, for the vias that
    /// went through the registry (`ExplicitName`, `RepoBinding`,
    /// `ReverseLookup`). `None` for the direct-path vias (`ExplicitPath`,
    /// `NornRootEnv`) and `UnregisteredCwd`, which never consult the
    /// registry. Carrying the matched entry here means a caller that needs
    /// one of its fields (e.g. a config override) reads it off this result
    /// instead of looking the name back up a second time.
    pub vault: Option<RegisteredVault>,
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
                vault: None,
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
                vault: None,
            });
        }

        // 5. cwd reverse lookup — through the registry, fail loud on stale.
        if let Some(vault) = self.reverse_lookup(&input.cwd)? {
            ensure_live(&vault)?;
            return Ok(resolved_from(vault, ResolvedVia::ReverseLookup));
        }

        // 6. Unregistered cwd — the ephemeral outcome, not an error.
        Ok(Resolved {
            root: canonicalize_best_effort(&input.cwd),
            name: None,
            via: ResolvedVia::UnregisteredCwd,
            vault: None,
        })
    }
}

fn resolved_from(vault: RegisteredVault, via: ResolvedVia) -> Resolved {
    Resolved {
        root: vault.root.clone(),
        name: Some(vault.name.clone()),
        via,
        vault: Some(vault),
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

/// Absolute paths pass through; relative ones are grounded against `cwd`. The
/// grounded path is then canonicalized (resolving `..`, `.`, and symlinks —
/// e.g. macOS's `/tmp` → `/private/tmp`) so every downstream consumer (an
/// applied plan's `vault_root`, the owner's runtime dir derivation, a
/// registry reverse lookup) sees one canonical spelling of the root
/// regardless of how `-C`/`NORN_ROOT` spelled it (NRN-415). A root that does
/// not exist yet cannot canonicalize (`fs::canonicalize` requires the path to
/// exist) — that case falls back to the grounded-but-uncanonicalized path,
/// preserving today's error surface: resolution itself never fails on a
/// missing root, the same as before this canonicalization was added; a
/// missing-root's user-facing classification is a separate concern (NRN-414).
fn ground(cwd: &Path, path: &Path) -> PathBuf {
    let grounded = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    canonicalize_best_effort(&grounded)
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
        // Canonical (NRN-415): the explicit path is grounded then
        // canonicalized, so on macOS a `/tmp`-rooted tempdir compares against
        // its `/private/...` canonical form, not the raw spelling.
        assert_eq!(resolved.root, explicit.canonicalize().unwrap());
    }

    #[test]
    fn registry_vias_carry_the_matched_entry_direct_vias_do_not() {
        // A registry-resolved via (`ExplicitName` here) carries the matched
        // `RegisteredVault` on `Resolved`, so a caller reads its config
        // override straight off the resolution instead of looking the name
        // back up a second time. Direct-path and unregistered vias never
        // consult the registry, so their `vault` stays `None`.
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let vault_dir = tmp.path().join("vault");
        fs::create_dir_all(&vault_dir).unwrap();
        let config_path = tmp.path().join("custom-config.yaml");
        reg.register(
            "docs",
            &vault_dir,
            VaultOverrides {
                config: Some(config_path.clone()),
                ..VaultOverrides::default()
            },
        )
        .unwrap();

        let input = ResolveInput {
            explicit_name: Some("docs".into()),
            ..ResolveInput::new(tmp.path())
        };
        let resolved = reg.resolve(&input).unwrap();
        let matched = resolved.vault.expect("registry via must carry the entry");
        assert_eq!(matched.name, "docs");
        assert_eq!(matched.config.as_deref(), Some(config_path.as_path()));

        let direct = reg
            .resolve(&ResolveInput {
                explicit_path: Some(vault_dir.clone()),
                ..ResolveInput::new(tmp.path())
            })
            .unwrap();
        assert_eq!(direct.vault, None);

        let elsewhere = tmp.path().join("elsewhere-unused");
        fs::create_dir_all(&elsewhere).unwrap();
        let unregistered = reg.resolve(&ResolveInput::new(elsewhere)).unwrap();
        assert_eq!(unregistered.via, ResolvedVia::UnregisteredCwd);
        assert_eq!(unregistered.vault, None);
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
        // A never-created tempdir child stays missing on every host, so the
        // canonicalize fallback (grounded path, uncanonicalized) is what this
        // pins; an existing NORN_ROOT canonicalizes like any other root.
        let missing_root = tmp.path().join("never-created-env-root");
        let input = ResolveInput {
            cwd: vault.clone(),
            norn_root_env: Some(missing_root.to_str().unwrap().into()),
            ..ResolveInput::new(vault.clone())
        };
        let resolved = reg.resolve(&input).unwrap();
        assert_eq!(resolved.via, ResolvedVia::NornRootEnv);
        assert_eq!(resolved.root, missing_root);

        let existing_root = tmp.path().join("existing-env-root");
        fs::create_dir_all(&existing_root).unwrap();
        let input = ResolveInput {
            cwd: vault.clone(),
            norn_root_env: Some(existing_root.to_str().unwrap().into()),
            ..ResolveInput::new(vault.clone())
        };
        let resolved = reg.resolve(&input).unwrap();
        assert_eq!(resolved.via, ResolvedVia::NornRootEnv);
        assert_eq!(resolved.root, existing_root.canonicalize().unwrap());
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

    #[test]
    #[cfg(unix)]
    fn ground_resolves_through_a_constructed_symlink() {
        // A symlink built deliberately for this test, not the incidental
        // macOS `/tmp` -> `/private/tmp` symlink the other tests ride on
        // (NRN-415 fix round) — pins `ground`'s canonicalization itself
        // rather than a platform accident.
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let target = tmp.path().join("real-target");
        fs::create_dir_all(&target).unwrap();
        let link = tmp.path().join("via-symlink");
        symlink(&target, &link).unwrap();

        let input = ResolveInput {
            explicit_path: Some(link),
            ..ResolveInput::new(tmp.path())
        };
        let resolved = reg.resolve(&input).unwrap();
        assert_eq!(resolved.via, ResolvedVia::ExplicitPath);
        assert_eq!(resolved.root, target.canonicalize().unwrap());
    }

    #[test]
    fn ground_normalizes_dot_component() {
        // `/path/./sub` must normalize to the same canonical form as
        // `/path/sub` — pins normalization on every platform, independent of
        // any incidental symlink in the tempdir's own path.
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let sub = tmp.path().join("sub");
        fs::create_dir_all(&sub).unwrap();
        let dotted = tmp.path().join(".").join("sub");

        let input = ResolveInput {
            explicit_path: Some(dotted),
            ..ResolveInput::new(tmp.path())
        };
        let resolved = reg.resolve(&input).unwrap();
        assert_eq!(resolved.via, ResolvedVia::ExplicitPath);
        assert_eq!(resolved.root, sub.canonicalize().unwrap());
    }
}
