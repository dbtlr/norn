#![forbid(unsafe_code)]
//! Deterministic Markdown fixture-vault generator: `(Profile, seed)` in,
//! byte-identical vault tree out. Zero runtime dependencies — the generator
//! synthesizes Markdown/YAML text directly and never shells out to or links
//! against any `norn` crate or binary; it produces *inputs* for norn,
//! independent of the system under test (ADR 0018 oracle-parity harness).
//!
//! See `crate::zoo` for the fixed-content document set and `crate::expansion`
//! for the seeded procedural document set. `crate::contract` owns the strings
//! the config and document emitters must agree on.

mod config;
mod contract;
mod dates;
mod expansion;
mod rng;
mod words;
mod yaml;
mod zoo;

pub mod testing;

pub use contract::{SENTINEL_CONTENT, SENTINEL_FILE};

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;

use expansion::KnownDoc;

/// A named generation profile: how much of the fixed zoo and how much
/// seeded procedural expansion `generate` produces.
#[derive(Clone, Copy)]
pub struct Profile {
    pub name: &'static str,
    /// Include the deliberate-violation zoo docs.
    pub violations: bool,
    /// Number of seeded expansion docs beyond the zoo.
    pub expansion_docs: usize,
    /// Max folder nesting depth for expansion docs (zoo has its own fixed layout).
    pub folder_depth: usize,
    /// Folders created per level (bounded fan-out).
    pub folder_width: usize,
    /// Outgoing body links per expansion doc: rng.range(max_links_per_doc + 1).
    pub max_links_per_doc: usize,
    /// Per-mille of expansion links that intentionally point nowhere.
    pub broken_link_per_mille: u32,
    /// Per-mille of expansion docs that carry one schema violation.
    pub violation_per_mille: u32,
    /// Emit the text-layer edge docs (`crate::zoo::text_edge_docs`) — the
    /// isolated BOM / code-opacity divergence probes (NRN-349 / NRN-350). Kept
    /// off every shared profile so those cases never perturb zoo/clean parity.
    pub text_edge: bool,
    /// Write a deliberately-invalid `.norn/config.yaml` (one unknown top-level
    /// key) instead of the valid block, so a config load fails with a stable
    /// "unknown field" error. Drives the malformed-config error-surface parity
    /// case (NRN-361); kept off every other profile so a broken config never
    /// perturbs a real read.
    pub malformed_config: bool,
    /// Emit the mutation-edge docs (`crate::zoo::mutate_edge_docs`) — the
    /// isolated null-/comment-only frontmatter probes (NRN-371). Kept off every
    /// shared profile so the decided mapping-promotion divergence never perturbs
    /// zoo/clean parity.
    pub mutate_edge: bool,
    /// Emit the section-edge docs (`crate::zoo::section_edge_docs`) — the
    /// isolated SETEXT / heading-at-EOF probes (NRN-437). Kept off every shared
    /// profile so the decided section-op corruption fix never perturbs zoo/clean
    /// parity.
    pub section_edge: bool,
    /// Emit the wikilink-edge docs (`crate::zoo::wikilink_edge_docs`) — the
    /// isolated embed / code-fence-shadow / caret-stem backlink probes
    /// (NRN-424 / NRN-431/432/433). Kept off every shared profile so the decided
    /// wikilink-rewriter corruption fixes never perturb zoo/clean parity.
    pub wikilink_edge: bool,
}

/// The named profiles, in fixed order — the single source for `by_name`,
/// `all`, `names`, and the per-profile accessors below.
const PROFILES: &[Profile] = &[
    // Zoo including the deliberate-violation docs; no seeded expansion.
    Profile {
        name: "zoo",
        violations: true,
        expansion_docs: 0,
        folder_depth: 0,
        folder_width: 0,
        max_links_per_doc: 0,
        broken_link_per_mille: 0,
        violation_per_mille: 0,
        text_edge: false,
        malformed_config: false,
        mutate_edge: false,
        section_edge: false,
        wikilink_edge: false,
    },
    // Zoo without violations, plus ~60 expansion docs. Invariant: the oracle's
    // `validate` reports zero findings against this profile.
    Profile {
        name: "clean",
        violations: false,
        expansion_docs: 60,
        folder_depth: 2,
        folder_width: 3,
        max_links_per_doc: 3,
        broken_link_per_mille: 0,
        violation_per_mille: 0,
        text_edge: false,
        malformed_config: false,
        mutate_edge: false,
        section_edge: false,
        wikilink_edge: false,
    },
    // Zoo including violations, plus ~120 expansion docs at elevated
    // violation/broken-link ratios.
    Profile {
        name: "violations",
        violations: true,
        expansion_docs: 120,
        folder_depth: 2,
        folder_width: 3,
        max_links_per_doc: 4,
        broken_link_per_mille: 40,
        violation_per_mille: 50,
        text_edge: false,
        malformed_config: false,
        mutate_edge: false,
        section_edge: false,
        wikilink_edge: false,
    },
    // Zoo without violations, plus ~200 densely-linked docs across deep, wide
    // folders.
    Profile {
        name: "linky",
        violations: false,
        expansion_docs: 200,
        folder_depth: 3,
        folder_width: 4,
        max_links_per_doc: 8,
        broken_link_per_mille: 0,
        violation_per_mille: 0,
        text_edge: false,
        malformed_config: false,
        mutate_edge: false,
        section_edge: false,
        wikilink_edge: false,
    },
    // Zoo without violations, plus 1000 expansion docs at moderate settings.
    Profile {
        name: "large",
        violations: false,
        expansion_docs: 1000,
        folder_depth: 3,
        folder_width: 5,
        max_links_per_doc: 3,
        broken_link_per_mille: 0,
        violation_per_mille: 0,
        text_edge: false,
        malformed_config: false,
        mutate_edge: false,
        section_edge: false,
        wikilink_edge: false,
    },
    // The valid zoo plus the text-layer edge probes (NRN-349 / NRN-350) — no
    // expansion, no violations. Dedicated to the BOM / code-opacity parity cases
    // so those divergences stay off every shared fixture.
    Profile {
        name: "text-edge",
        violations: false,
        expansion_docs: 0,
        folder_depth: 0,
        folder_width: 0,
        max_links_per_doc: 0,
        broken_link_per_mille: 0,
        violation_per_mille: 0,
        text_edge: true,
        malformed_config: false,
        mutate_edge: false,
        section_edge: false,
        wikilink_edge: false,
    },
    // The valid zoo doc tree under a deliberately-INVALID `.norn/config.yaml`.
    // Dedicated to the malformed-config error-surface parity case (NRN-361): the
    // vault warms into a config rejection, so the read never reaches the graph —
    // keeping the broken config off every profile that performs a real read.
    Profile {
        name: "bad-config",
        violations: false,
        expansion_docs: 0,
        folder_depth: 0,
        folder_width: 0,
        max_links_per_doc: 0,
        broken_link_per_mille: 0,
        violation_per_mille: 0,
        text_edge: false,
        malformed_config: true,
        mutate_edge: false,
        section_edge: false,
        wikilink_edge: false,
    },
    // The valid zoo doc tree plus the mutation-edge probes (NRN-371) — no
    // expansion, no violations. Dedicated to the null-/comment-only frontmatter
    // parity cases so the decided mapping-promotion divergence stays off every
    // shared fixture (mirrors `text-edge`).
    Profile {
        name: "mutate-edge",
        violations: false,
        expansion_docs: 0,
        folder_depth: 0,
        folder_width: 0,
        max_links_per_doc: 0,
        broken_link_per_mille: 0,
        violation_per_mille: 0,
        text_edge: false,
        malformed_config: false,
        mutate_edge: true,
        section_edge: false,
        wikilink_edge: false,
    },
    // The valid zoo doc tree plus the section-edge probes (NRN-437) — no
    // expansion, no violations. Dedicated to the SETEXT / heading-at-EOF section
    // parity cases so the decided body-transform corruption fix stays off every
    // shared fixture (mirrors `text-edge` / `mutate-edge`).
    Profile {
        name: "section-edge",
        violations: false,
        expansion_docs: 0,
        folder_depth: 0,
        folder_width: 0,
        max_links_per_doc: 0,
        broken_link_per_mille: 0,
        violation_per_mille: 0,
        text_edge: false,
        malformed_config: false,
        mutate_edge: false,
        section_edge: true,
        wikilink_edge: false,
    },
    // The valid zoo doc tree plus the wikilink-edge probes (NRN-424) — no
    // expansion, no violations. Dedicated to the embed / code-fence-shadow /
    // caret-stem backlink parity cases so the decided wikilink-rewriter
    // corruption fixes stay off every shared fixture (mirrors the other -edge
    // profiles).
    Profile {
        name: "wikilink-edge",
        violations: false,
        expansion_docs: 0,
        folder_depth: 0,
        folder_width: 0,
        max_links_per_doc: 0,
        broken_link_per_mille: 0,
        violation_per_mille: 0,
        text_edge: false,
        malformed_config: false,
        mutate_edge: false,
        section_edge: false,
        wikilink_edge: true,
    },
];

impl Profile {
    /// Zoo including the deliberate-violation docs; no seeded expansion.
    pub fn zoo() -> Profile {
        Self::named("zoo")
    }

    /// Zoo without violations, plus ~60 expansion docs. Invariant: the
    /// oracle's `validate` reports zero findings against this profile.
    pub fn clean() -> Profile {
        Self::named("clean")
    }

    /// Zoo including violations, plus ~120 expansion docs at elevated
    /// violation/broken-link ratios.
    pub fn violations() -> Profile {
        Self::named("violations")
    }

    /// Zoo without violations, plus ~200 densely-linked docs across deep,
    /// wide folders.
    pub fn linky() -> Profile {
        Self::named("linky")
    }

    /// Zoo without violations, plus 1000 expansion docs at moderate settings.
    pub fn large() -> Profile {
        Self::named("large")
    }

    /// Look up a named profile, for the bin's `--profile` flag.
    pub fn by_name(name: &str) -> Option<Profile> {
        PROFILES.iter().copied().find(|p| p.name == name)
    }

    /// All named profiles, in a fixed order — for `--help`-style listings.
    pub fn all() -> &'static [Profile] {
        PROFILES
    }

    /// Names of all named profiles, in the same fixed order as `all()`.
    pub fn names() -> Vec<&'static str> {
        PROFILES.iter().map(|p| p.name).collect()
    }

    /// Internal: the table entry that must exist by construction.
    fn named(name: &'static str) -> Profile {
        Self::by_name(name).expect("named profile present in PROFILES table")
    }
}

/// Which validation tier a manifest doc exercises.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// Validates cleanly (or is a valid link/structure fixture).
    Valid,
    /// Trips at least one finding.
    Violation,
    /// In the graph but exempt from validation (`validate.ignore`).
    ValidateIgnored,
    /// Dropped from the graph entirely (`files.ignore`).
    FilesIgnored,
}

/// One emitted markdown document in the manifest.
pub struct DocEntry {
    /// Vault-relative path (forward-slash).
    pub path: String,
    /// The validation tier this doc exercises.
    pub tier: Tier,
    /// Finding codes the oracle is expected to report for this doc; empty for
    /// a clean doc.
    pub codes: &'static [&'static str],
}

/// The inventory of a generated vault: every emitted markdown doc plus the
/// file/dir totals. `Summary`'s counts derive from this.
pub struct Manifest {
    /// Every emitted markdown doc, in emission order.
    pub docs: Vec<DocEntry>,
    /// Total files written, including `.norn/config.yaml`, binary assets,
    /// and the sentinel.
    pub files: usize,
    /// Directories created.
    pub dirs: usize,
}

impl Manifest {
    /// The count-only view used by the bin's stdout line.
    pub fn summary(&self) -> Summary {
        Summary {
            docs: self.docs.len(),
            files: self.files,
            dirs: self.dirs,
        }
    }

    /// The union of every expected finding code across all docs.
    pub fn expected_codes(&self) -> BTreeSet<&'static str> {
        self.docs
            .iter()
            .flat_map(|d| d.codes.iter().copied())
            .collect()
    }
}

/// Counts describing a generated vault, derived from the [`Manifest`].
pub struct Summary {
    /// Markdown documents written (zoo + expansion, regardless of ignore tier).
    pub docs: usize,
    /// Total files written, including `.norn/config.yaml`, binary assets,
    /// and the sentinel.
    pub files: usize,
    /// Directories created.
    pub dirs: usize,
}

/// Track every ancestor directory of `rel_path` (forward-slash vault-relative
/// string) in `dirs`, without touching the filesystem.
fn track_dirs(rel_path: &str, dirs: &mut BTreeSet<String>) {
    let mut acc: Vec<&str> = Vec::new();
    let parts: Vec<&str> = rel_path.split('/').collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        acc.push(part);
        dirs.insert(acc.join("/"));
    }
}

fn write_rel(
    out_dir: &Path,
    rel_path: &str,
    bytes: &[u8],
    dirs: &mut BTreeSet<String>,
    files: &mut usize,
) -> io::Result<()> {
    let full = out_dir.join(rel_path);
    // Only touch the filesystem for a parent directory we have not created
    // yet — once a dir is tracked, every ancestor of it is tracked too, so a
    // fresh `create_dir_all` would be a redundant syscall.
    if let Some((parent_rel, _)) = rel_path.rsplit_once('/') {
        if !dirs.contains(parent_rel) {
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent)?;
            }
        }
    }
    fs::write(&full, bytes)?;
    track_dirs(rel_path, dirs);
    *files += 1;
    Ok(())
}

/// Generate a fixture vault at `out_dir` per `profile` and `seed`, returning
/// its [`Manifest`].
///
/// `out_dir` must not exist, or must be an empty directory — anything else
/// is an error. Writes the sentinel file `.norn-fixture-vault` into
/// `out_dir` on success (hidden — invisible to norn's graph).
pub fn generate(profile: &Profile, seed: u64, out_dir: &Path) -> io::Result<Manifest> {
    match fs::metadata(out_dir) {
        Ok(meta) => {
            if !meta.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not a directory", out_dir.display()),
                ));
            }
            if fs::read_dir(out_dir)?.next().is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not empty", out_dir.display()),
                ));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(out_dir)?;
        }
        Err(e) => return Err(e),
    }

    let mut dirs = BTreeSet::new();
    let mut files = 0usize;
    let mut docs: Vec<DocEntry> = Vec::new();

    let config_yaml = if profile.malformed_config {
        config::malformed_config_yaml()
    } else {
        config::config_yaml()
    };
    write_rel(
        out_dir,
        ".norn/config.yaml",
        config_yaml.as_bytes(),
        &mut dirs,
        &mut files,
    )?;

    let mut known: Vec<KnownDoc> = Vec::new();
    for doc in zoo::valid_docs() {
        write_rel(
            out_dir,
            doc.path,
            doc.content.as_bytes(),
            &mut dirs,
            &mut files,
        )?;
        docs.push(DocEntry {
            path: doc.path.to_string(),
            tier: doc.tier,
            codes: &[],
        });
        if doc.linkable {
            let stem = Path::new(doc.path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            known.push(KnownDoc::new(stem.to_string(), doc.path));
        }
    }
    for (path, bytes) in zoo::binary_docs() {
        write_rel(out_dir, path, bytes, &mut dirs, &mut files)?;
    }

    if profile.text_edge {
        for doc in zoo::text_edge_docs() {
            write_rel(
                out_dir,
                doc.path,
                doc.content.as_bytes(),
                &mut dirs,
                &mut files,
            )?;
            docs.push(DocEntry {
                path: doc.path.to_string(),
                tier: doc.tier,
                codes: &[],
            });
        }
    }

    if profile.mutate_edge {
        for doc in zoo::mutate_edge_docs() {
            write_rel(
                out_dir,
                doc.path,
                doc.content.as_bytes(),
                &mut dirs,
                &mut files,
            )?;
            docs.push(DocEntry {
                path: doc.path.to_string(),
                tier: doc.tier,
                codes: &[],
            });
        }
    }

    if profile.section_edge {
        for doc in zoo::section_edge_docs() {
            write_rel(
                out_dir,
                doc.path,
                doc.content.as_bytes(),
                &mut dirs,
                &mut files,
            )?;
            docs.push(DocEntry {
                path: doc.path.to_string(),
                tier: doc.tier,
                codes: &[],
            });
        }
    }

    if profile.wikilink_edge {
        for doc in zoo::wikilink_edge_docs() {
            write_rel(
                out_dir,
                doc.path,
                doc.content.as_bytes(),
                &mut dirs,
                &mut files,
            )?;
            docs.push(DocEntry {
                path: doc.path.to_string(),
                tier: doc.tier,
                codes: &[],
            });
        }
    }

    if profile.violations {
        for doc in zoo::violation_docs() {
            write_rel(
                out_dir,
                doc.path,
                doc.content.as_bytes(),
                &mut dirs,
                &mut files,
            )?;
            docs.push(DocEntry {
                path: doc.path.to_string(),
                tier: Tier::Violation,
                codes: doc.codes,
            });
        }
    }

    if profile.expansion_docs > 0 {
        for doc in expansion::generate(profile, seed, known) {
            write_rel(
                out_dir,
                &doc.path,
                doc.content.as_bytes(),
                &mut dirs,
                &mut files,
            )?;
            let tier = if doc.codes.is_empty() {
                Tier::Valid
            } else {
                Tier::Violation
            };
            docs.push(DocEntry {
                path: doc.path,
                tier,
                codes: doc.codes,
            });
        }
    }

    // Sentinel: written last, single line, hidden — kept out of norn's graph.
    write_rel(
        out_dir,
        SENTINEL_FILE,
        SENTINEL_CONTENT.as_bytes(),
        &mut dirs,
        &mut files,
    )?;

    Ok(Manifest {
        docs,
        files,
        dirs: dirs.len(),
    })
}
