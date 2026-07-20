//! Fixture materialization: `norn-fixtures` generations under a per-run temp
//! root, addressed by path.
//!
//! Per-SIDE vaults are the point: the oracle and candidate binaries each get
//! their OWN copy of every fixture. Sharing one on-disk vault let the oracle
//! run warm/rewrite the cache and the candidate then observe oracle-mutated
//! state (a cache-rebuild stderr line the candidate never earned). Because
//! generation is deterministic, the two copies are byte-identical, so each
//! side accumulates only its own binary's cache state across cases, in the
//! same order — symmetric by construction.
//!
//! Two granularities of copy, chosen per case by [`FixtureCache::materialize`]:
//!
//! - **Read cases** (`mutating: false`) share ONE cached copy per (fixture,
//!   side) — they only read, so reuse is safe and avoids regenerating a
//!   fixture for every case that touches it.
//! - **Mutating cases** (`mutating: true`) get a FRESH per-case copy per side.
//!   The case writes to the vault, so a shared copy would leak its writes into
//!   the next case and desynchronize the two sides' pre-state. A fresh copy
//!   per (fixture, side, case) keeps each case hermetic and starts oracle and
//!   candidate from byte-identical pre-state, which is what makes the two
//!   post-mutation trees comparable (see `crate::poststate`).

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use norn_fixtures::{Manifest, Profile};
use tempfile::TempDir;

use crate::cases::Fixture;

/// A materialized fixture vault ready to run one case against: the vault
/// directory to use as the process cwd, plus every absolute spelling of it
/// (canonical + pre-canonical alias). Returned by [`FixtureCache::materialize`].
pub struct Vault {
    pub path: PathBuf,
    pub spellings: Vec<PathBuf>,
}

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

    /// The per-(fixture, side) directory a read case's SHARED vault nests
    /// under — `<profile>-<seed>-<side>/vault`.
    fn rel(fixture: &Fixture, side: Side) -> PathBuf {
        PathBuf::from(format!(
            "{}-{}-{}",
            fixture.profile_name,
            fixture.seed,
            side.tag()
        ))
        .join("vault")
    }

    /// The per-(fixture, side, case) directory a MUTATING case's fresh vault
    /// nests under — `<profile>-<seed>-<side>-<case>/vault`. The case id keeps
    /// it distinct from both the shared read-case copy (`rel`) and every other
    /// mutating case's copy, so one case's writes never reach another.
    fn rel_fresh(fixture: &Fixture, side: Side, case_id: &str) -> PathBuf {
        PathBuf::from(format!(
            "{}-{}-{}-{}",
            fixture.profile_name,
            fixture.seed,
            side.tag(),
            case_id
        ))
        .join("vault")
    }

    /// Deterministically generate `fixture` into `<canonical_root>/<rel>`,
    /// recording its manifest (identical across sides + copies, so the first
    /// generation wins), and return the vault directory.
    ///
    /// Vault directories nest one level below a per-copy directory so the
    /// vault root's own basename never starts with `.` regardless of the
    /// platform's temp-dir naming — `norn` treats a dot-prefixed vault root as
    /// fully hidden (empty graph), a behavior `norn-fixtures`' own test
    /// helpers dodge the same way (see `crates/norn-fixtures/tests/common/mod.rs`).
    fn generate_into(&mut self, fixture: &Fixture, rel: &Path) -> Result<PathBuf, FixtureError> {
        let profile =
            Profile::by_name(fixture.profile_name).ok_or(FixtureError::UnknownProfile {
                name: fixture.profile_name,
            })?;
        let vault = self.canonical_root.join(rel);
        let manifest =
            norn_fixtures::generate(&profile, fixture.seed, &vault).map_err(|source| {
                FixtureError::Generation {
                    profile: fixture.profile_name,
                    seed: fixture.seed,
                    source,
                }
            })?;
        self.manifests.entry(*fixture).or_insert(manifest);
        Ok(vault)
    }

    /// The shared read-case vault directory for `fixture` on `side`,
    /// generating it once and caching it. Reused across every read case on
    /// that fixture (whose runs only read, so the shared copy is safe) and by
    /// the oracle-only consistency checks.
    pub fn vault_for(&mut self, fixture: &Fixture, side: Side) -> Result<PathBuf, FixtureError> {
        if let Some(path) = self.vaults.get(&(*fixture, side)) {
            return Ok(path.clone());
        }
        let vault = self.generate_into(fixture, &Self::rel(fixture, side))?;
        self.vaults.insert((*fixture, side), vault.clone());
        Ok(vault)
    }

    /// Materialize the vault a single case runs against on `side`.
    ///
    /// - `fresh_for: None` (a read case) → the shared, cached per-(fixture,
    ///   side) copy ([`Self::vault_for`]).
    /// - `fresh_for: Some(case_id)` (a mutating case) → a fresh copy generated
    ///   into a per-case directory, NEVER cached, so the case's writes cannot
    ///   contaminate any other case and both sides begin from byte-identical
    ///   pre-state. Cost: one extra deterministic generation per mutating case
    ///   per side; the fixtures are small, so this is cheap next to the two
    ///   binary spawns the case already pays. (Copying the cached tree would
    ///   also work; regeneration is chosen for being the simpler single path.)
    ///
    /// Returns the vault directory to use as the process cwd plus every
    /// absolute spelling of it, so post-normalization strips whichever a
    /// binary echoes — the one call the runner needs per (case, side),
    /// replacing the former `vault_for` + `vault_spellings` pair.
    pub fn materialize(
        &mut self,
        fixture: &Fixture,
        side: Side,
        fresh_for: Option<&str>,
    ) -> Result<Vault, FixtureError> {
        let (path, rel) = match fresh_for {
            None => (self.vault_for(fixture, side)?, Self::rel(fixture, side)),
            Some(case_id) => {
                let rel = Self::rel_fresh(fixture, side, case_id);
                (self.generate_into(fixture, &rel)?, rel)
            }
        };
        let spellings = self.spellings_at(&rel);
        Ok(Vault { path, spellings })
    }

    /// Materialize an authored-plan [`Case`](crate::cases::Case)'s plan file for
    /// one side: substitute every [`crate::cases::PLAN_VAULT_ROOT_TOKEN`] in
    /// `template` with `vault`'s own absolute path (the pinned oracle rejects a
    /// plan whose `vault_root` does not canonicalize to the invoked cwd), and
    /// write the result to a file dedicated to this `(fixture, side, case_id)` —
    /// deliberately a SIBLING of the vault directory, never inside it, so the
    /// plan file is invisible to `crate::poststate`'s tree snapshot and to
    /// `norn`'s own vault scan. Returns the plan file's absolute path, for
    /// substitution into the case's argv (the [`crate::cases::PLAN_ARGV_PLACEHOLDER`]
    /// token).
    pub fn materialize_plan(
        &self,
        fixture: &Fixture,
        side: Side,
        case_id: &str,
        vault: &Path,
        template: &str,
    ) -> Result<PathBuf, FixtureError> {
        let dir = self.canonical_root.join(format!(
            "{}-{}-{}-{}-plan",
            fixture.profile_name,
            fixture.seed,
            side.tag(),
            case_id
        ));
        std::fs::create_dir_all(&dir).map_err(|source| FixtureError::PlanWrite {
            path: dir.clone(),
            source,
        })?;
        // The token sits inside a JSON string in the template, so the path is
        // JSON-escaped before substitution — a root containing `"`, `\`, or a
        // control character must not corrupt the plan (temp roots never do
        // today; this keeps the harness correct if that ever changes).
        let escaped = serde_json::to_string(&vault.display().to_string())
            .expect("a path string always serializes");
        let content = template.replace(
            crate::cases::PLAN_VAULT_ROOT_TOKEN,
            escaped.trim_matches('"'),
        );
        let path = dir.join("plan.json");
        std::fs::write(&path, content).map_err(|source| FixtureError::PlanWrite {
            path: path.clone(),
            source,
        })?;
        Ok(path)
    }

    /// Every valid absolute spelling of the vault at `rel` — the canonical
    /// path plus the pre-canonical alias — so normalization strips whichever a
    /// binary happens to echo (see [`Self::new`]).
    fn spellings_at(&self, rel: &Path) -> Vec<PathBuf> {
        let canonical = self.canonical_root.join(rel);
        let precanonical = self.root_path.join(rel);
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
    /// An authored-plan case's plan file (`FixtureCache::materialize_plan`)
    /// could not be written — an environment/IO failure, not a verdict, so the
    /// run aborts rather than guessing at a comparison.
    PlanWrite {
        path: PathBuf,
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
            FixtureError::PlanWrite { path, source } => {
                write!(f, "failed to write authored plan file {path:?}: {source}")
            }
        }
    }
}

impl std::error::Error for FixtureError {}
