//! Fixture materialization: one `norn-fixtures` generation per distinct
//! `(profile, seed)` per run, cached by path so every case sharing a
//! fixture reuses the same on-disk vault.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

use norn_fixtures::Profile;
use tempfile::TempDir;

use crate::cases::Fixture;

/// Materializes and caches fixture vaults for one run. Held for the
/// lifetime of the run so its backing [`TempDir`] is not dropped (and
/// cleaned up) before every case has finished reading from it.
pub struct FixtureCache {
    _root: TempDir,
    root_path: PathBuf,
    vaults: BTreeMap<Fixture, PathBuf>,
}

impl FixtureCache {
    pub fn new() -> Result<Self, FixtureError> {
        let root = TempDir::new().map_err(FixtureError::TempRoot)?;
        let root_path = root.path().to_path_buf();
        Ok(Self {
            _root: root,
            root_path,
            vaults: BTreeMap::new(),
        })
    }

    /// The vault directory for `fixture`, generating it on first request.
    ///
    /// Vault directories nest one level below a per-fixture directory
    /// (`<root>/<profile>-<seed>/vault`) so the vault root's own basename
    /// never starts with `.` regardless of the platform's temp-dir naming —
    /// `norn` treats a dot-prefixed vault root as fully hidden (empty
    /// graph), a behavior `norn-fixtures`' own test helpers dodge the same
    /// way (see `crates/norn-fixtures/tests/common/mod.rs`).
    pub fn vault_for(&mut self, fixture: &Fixture) -> Result<PathBuf, FixtureError> {
        if let Some(path) = self.vaults.get(fixture) {
            return Ok(path.clone());
        }
        let profile =
            Profile::by_name(fixture.profile_name).ok_or(FixtureError::UnknownProfile {
                name: fixture.profile_name,
            })?;
        let vault = self
            .root_path
            .join(format!("{}-{}", fixture.profile_name, fixture.seed))
            .join("vault");
        norn_fixtures::generate(&profile, fixture.seed, &vault).map_err(|source| {
            FixtureError::Generation {
                profile: fixture.profile_name,
                seed: fixture.seed,
                source,
            }
        })?;
        self.vaults.insert(*fixture, vault.clone());
        Ok(vault)
    }
}

#[derive(Debug)]
pub enum FixtureError {
    TempRoot(io::Error),
    UnknownProfile {
        name: &'static str,
    },
    Generation {
        profile: &'static str,
        seed: u64,
        source: io::Error,
    },
}

impl std::fmt::Display for FixtureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FixtureError::TempRoot(source) => {
                write!(
                    f,
                    "failed to create a temp root for fixture vaults: {source}"
                )
            }
            FixtureError::UnknownProfile { name } => {
                write!(f, "unknown fixture profile: {name}")
            }
            FixtureError::Generation {
                profile,
                seed,
                source,
            } => write!(
                f,
                "fixture generation failed for profile {profile} seed {seed}: {source}"
            ),
        }
    }
}

impl std::error::Error for FixtureError {}
