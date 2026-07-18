//! Fixture materialization: one `norn-fixtures` generation per distinct
//! `(profile, seed, side)` per run, cached by path.
//!
//! Per-SIDE vaults are the point: the oracle and candidate binaries each get
//! their OWN copy of every fixture. Sharing one on-disk vault let the oracle
//! run warm/rewrite the cache and the candidate then observe oracle-mutated
//! state (a cache-rebuild stderr line the candidate never earned). Because
//! generation is deterministic, the two copies are byte-identical, so each
//! side accumulates only its own binary's cache state across cases, in the
//! same order — symmetric by construction.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

use norn_fixtures::{Manifest, Profile};
use tempfile::TempDir;

use crate::cases::Fixture;

/// Which binary a materialized vault belongs to. Each side is a separate
/// on-disk copy so neither binary's cache state leaks into the other's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Side {
    Oracle,
    Candidate,
}

impl Side {
    fn tag(self) -> &'static str {
        match self {
            Side::Oracle => "oracle",
            Side::Candidate => "candidate",
        }
    }
}

/// Materializes and caches fixture vaults for one run. Held for the
/// lifetime of the run so its backing [`TempDir`] is not dropped (and
/// cleaned up) before every case has finished reading from it.
pub struct FixtureCache {
    _root: TempDir,
    /// The temp root as `TempDir` reports it — on macOS a `/var/folders/...`
    /// symlink alias of `canonical_root`.
    root_path: PathBuf,
    /// The canonicalized temp root (`/private/var/folders/...` on macOS).
    /// Vaults are generated under this so the path a binary sees as its cwd
    /// is already canonical, and normalization is given both spellings.
    canonical_root: PathBuf,
    vaults: BTreeMap<(Fixture, Side), PathBuf>,
    /// Manifests keyed by fixture (both sides generate identical trees, so
    /// one manifest per fixture suffices) — used to check a case's
    /// `requires_doc` / `requires_code` against what was actually generated.
    manifests: BTreeMap<Fixture, Manifest>,
}

impl FixtureCache {
    pub fn new() -> Result<Self, FixtureError> {
        let root = TempDir::new().map_err(FixtureError::TempRoot)?;
        let root_path = root.path().to_path_buf();
        let canonical_root = root_path
            .canonicalize()
            .unwrap_or_else(|_| root_path.clone());
        Ok(Self {
            _root: root,
            root_path,
            canonical_root,
            vaults: BTreeMap::new(),
            manifests: BTreeMap::new(),
        })
    }

    fn rel(fixture: &Fixture, side: Side) -> PathBuf {
        PathBuf::from(format!(
            "{}-{}-{}",
            fixture.profile_name,
            fixture.seed,
            side.tag()
        ))
        .join("vault")
    }

    /// The vault directory (canonical spelling) for `fixture` on `side`,
    /// generating it on first request.
    ///
    /// Vault directories nest one level below a per-(fixture, side) directory
    /// (`<root>/<profile>-<seed>-<side>/vault`) so the vault root's own
    /// basename never starts with `.` regardless of the platform's temp-dir
    /// naming — `norn` treats a dot-prefixed vault root as fully hidden
    /// (empty graph), a behavior `norn-fixtures`' own test helpers dodge the
    /// same way (see `crates/norn-fixtures/tests/common/mod.rs`).
    pub fn vault_for(&mut self, fixture: &Fixture, side: Side) -> Result<PathBuf, FixtureError> {
        if let Some(path) = self.vaults.get(&(*fixture, side)) {
            return Ok(path.clone());
        }
        let profile =
            Profile::by_name(fixture.profile_name).ok_or(FixtureError::UnknownProfile {
                name: fixture.profile_name,
            })?;
        let vault = self.canonical_root.join(Self::rel(fixture, side));
        let manifest =
            norn_fixtures::generate(&profile, fixture.seed, &vault).map_err(|source| {
                FixtureError::Generation {
                    profile: fixture.profile_name,
                    seed: fixture.seed,
                    source,
                }
            })?;
        self.manifests.entry(*fixture).or_insert(manifest);
        self.vaults.insert((*fixture, side), vault.clone());
        Ok(vault)
    }

    /// Every valid absolute spelling of `fixture`'s `side` vault — the
    /// canonical path plus the pre-canonical alias — so normalization strips
    /// whichever a binary happens to echo (see [`Self::new`]).
    pub fn vault_spellings(&self, fixture: &Fixture, side: Side) -> Vec<PathBuf> {
        let rel = Self::rel(fixture, side);
        let canonical = self.canonical_root.join(&rel);
        let precanonical = self.root_path.join(&rel);
        if canonical == precanonical {
            vec![canonical]
        } else {
            vec![canonical, precanonical]
        }
    }

    /// The generated manifest for `fixture` — present only after
    /// [`Self::vault_for`] has materialized at least one side of it.
    pub fn manifest_for(&self, fixture: &Fixture) -> Option<&Manifest> {
        self.manifests.get(fixture)
    }
}

/// Check a case's declared requirements against what the fixture actually
/// generated. Returns the unmet requirement (if any) so the caller can raise
/// a runner error naming the case.
pub fn unmet_requirement(
    manifest: &Manifest,
    requires_doc: Option<&str>,
    requires_code: Option<&str>,
) -> Option<Requirement> {
    if let Some(doc) = requires_doc {
        if !manifest.docs.iter().any(|d| d.path == doc) {
            return Some(Requirement::Doc(doc.to_string()));
        }
    }
    if let Some(code) = requires_code {
        if !manifest.expected_codes().contains(code) {
            return Some(Requirement::Code(code.to_string()));
        }
    }
    None
}

/// An unmet case requirement — surfaced as a runner error, never a verdict.
#[derive(Debug, PartialEq, Eq)]
pub enum Requirement {
    Doc(String),
    Code(String),
}

impl std::fmt::Display for Requirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Requirement::Doc(p) => write!(f, "required doc `{p}` (not in the generated fixture)"),
            Requirement::Code(c) => write!(
                f,
                "required finding code `{c}` (not among the fixture's expected codes)"
            ),
        }
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
