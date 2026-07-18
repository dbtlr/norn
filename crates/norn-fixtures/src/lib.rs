#![forbid(unsafe_code)]
//! Deterministic Markdown fixture-vault generator: `(Profile, seed)` in,
//! byte-identical vault tree out. Zero runtime dependencies — the generator
//! synthesizes Markdown/YAML text directly and never shells out to or links
//! against any `norn` crate or binary; it produces *inputs* for norn,
//! independent of the system under test (ADR 0018 oracle-parity harness).
//!
//! See `crate::zoo` for the fixed-content document set and `crate::expansion`
//! for the seeded procedural document set.

mod config;
mod dates;
mod expansion;
mod rng;
mod words;
mod zoo;

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;

use expansion::KnownDoc;

/// A named generation profile: how much of the fixed zoo and how much
/// seeded procedural expansion `generate` produces.
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
}

impl Profile {
    /// Zoo including the deliberate-violation docs; no seeded expansion.
    pub fn zoo() -> Self {
        Profile {
            name: "zoo",
            violations: true,
            expansion_docs: 0,
            folder_depth: 0,
            folder_width: 0,
            max_links_per_doc: 0,
            broken_link_per_mille: 0,
            violation_per_mille: 0,
        }
    }

    /// Zoo without violations, plus ~60 expansion docs. Invariant: the
    /// oracle's `validate` reports zero findings against this profile.
    pub fn clean() -> Self {
        Profile {
            name: "clean",
            violations: false,
            expansion_docs: 60,
            folder_depth: 2,
            folder_width: 3,
            max_links_per_doc: 3,
            broken_link_per_mille: 0,
            violation_per_mille: 0,
        }
    }

    /// Zoo including violations, plus ~120 expansion docs at elevated
    /// violation/broken-link ratios.
    pub fn violations() -> Self {
        Profile {
            name: "violations",
            violations: true,
            expansion_docs: 120,
            folder_depth: 2,
            folder_width: 3,
            max_links_per_doc: 4,
            broken_link_per_mille: 40,
            violation_per_mille: 50,
        }
    }

    /// Zoo without violations, plus ~200 densely-linked docs across deep,
    /// wide folders.
    pub fn linky() -> Self {
        Profile {
            name: "linky",
            violations: false,
            expansion_docs: 200,
            folder_depth: 3,
            folder_width: 4,
            max_links_per_doc: 8,
            broken_link_per_mille: 0,
            violation_per_mille: 0,
        }
    }

    /// Zoo without violations, plus 1000 expansion docs at moderate settings.
    pub fn large() -> Self {
        Profile {
            name: "large",
            violations: false,
            expansion_docs: 1000,
            folder_depth: 3,
            folder_width: 5,
            max_links_per_doc: 3,
            broken_link_per_mille: 0,
            violation_per_mille: 0,
        }
    }

    /// Look up a named profile, for the bin's `--profile` flag.
    pub fn by_name(name: &str) -> Option<Profile> {
        match name {
            "zoo" => Some(Profile::zoo()),
            "clean" => Some(Profile::clean()),
            "violations" => Some(Profile::violations()),
            "linky" => Some(Profile::linky()),
            "large" => Some(Profile::large()),
            _ => None,
        }
    }

    /// All named profiles, in a fixed order — for `--help`-style listings.
    pub fn all() -> Vec<Profile> {
        vec![
            Profile::zoo(),
            Profile::clean(),
            Profile::violations(),
            Profile::linky(),
            Profile::large(),
        ]
    }

    /// Names of all named profiles, in the same fixed order as `all()`.
    pub fn names() -> Vec<&'static str> {
        Profile::all().into_iter().map(|p| p.name).collect()
    }
}

/// Counts describing a generated vault.
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
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&full, bytes)?;
    track_dirs(rel_path, dirs);
    *files += 1;
    Ok(())
}

/// Generate a fixture vault at `out_dir` per `profile` and `seed`.
///
/// `out_dir` must not exist, or must be an empty directory — anything else
/// is an error. Writes the sentinel file `.norn-fixture-vault` into
/// `out_dir` on success (hidden — invisible to norn's graph).
pub fn generate(profile: &Profile, seed: u64, out_dir: &Path) -> io::Result<Summary> {
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
    let mut docs = 0usize;

    write_rel(
        out_dir,
        ".norn/config.yaml",
        config::CONFIG_YAML.as_bytes(),
        &mut dirs,
        &mut files,
    )?;

    let mut known: Vec<KnownDoc> = Vec::new();
    for (path, content) in zoo::valid_docs() {
        write_rel(out_dir, path, content.as_bytes(), &mut dirs, &mut files)?;
        docs += 1;
        // "duplicate" is a deliberately-ambiguous stem (two docs share it);
        // excluding it from the expansion link-target pool keeps expansion
        // links from accidentally introducing an unintended ambiguous-link
        // finding into the clean/linky/large profiles.
        // "hidden-away" is files.ignore'd — not a valid link target either.
        let stem = Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if stem != "duplicate" && stem != "hidden-away" {
            known.push(KnownDoc {
                link_stem: stem.to_string(),
                link_path: path.trim_end_matches(".md").to_string(),
            });
        }
    }
    for (path, bytes) in zoo::binary_docs() {
        write_rel(out_dir, path, bytes, &mut dirs, &mut files)?;
    }

    if profile.violations {
        for (path, content) in zoo::violation_docs() {
            write_rel(out_dir, path, content.as_bytes(), &mut dirs, &mut files)?;
            docs += 1;
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
            docs += 1;
        }
    }

    // Sentinel: written last, single line, hidden — kept out of norn's graph.
    write_rel(
        out_dir,
        ".norn-fixture-vault",
        b"norn-fixtures generated vault \xe2\x80\x94 safe to delete\n",
        &mut dirs,
        &mut files,
    )?;

    Ok(Summary {
        docs,
        files,
        dirs: dirs.len(),
    })
}
