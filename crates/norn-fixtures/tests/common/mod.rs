//! Shared test helpers.
//!
//! Placed at `tests/common/mod.rs` (not `tests/common.rs`) so cargo treats it
//! as a module included by each integration test rather than as its own test
//! binary. Each test file does `mod common;` and uses a subset of these, so
//! the module carries `#![allow(dead_code)]`.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use norn_fixtures::{generate, Manifest, Profile};
use tempfile::TempDir;

/// Generate a fixture vault into a `vault/` subdirectory of a fresh `TempDir`,
/// returning the tempdir guard, the vault path, and the manifest.
///
/// The `vault/` subdir is load-bearing for the oracle: `norn` treats a vault
/// root whose own basename starts with `.` as fully hidden (an empty graph),
/// and `TempDir` names are dot-prefixed on this platform. Generating one level
/// down dodges that. Every test file generates the same way so the oracle and
/// the byte-determinism / inventory checks all see the same tree shape.
pub fn generate_vault(profile: &Profile, seed: u64) -> (TempDir, PathBuf, Manifest) {
    let dir = TempDir::new().unwrap();
    let vault = dir.path().join("vault");
    let manifest = generate(profile, seed, &vault).unwrap();
    (dir, vault, manifest)
}

/// Walk `root`, returning a sorted map of vault-relative path -> file bytes.
pub fn walk(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                stack.push(path);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    out
}

/// Count the markdown files under `root`.
pub fn count_md_files(root: &Path) -> usize {
    walk(root).keys().filter(|rel| rel.ends_with(".md")).count()
}
