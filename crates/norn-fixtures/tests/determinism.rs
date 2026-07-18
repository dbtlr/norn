//! Byte-determinism: the same `(profile, seed)` must produce a
//! byte-identical vault tree, and a different seed must change at least one
//! byte somewhere (sanity that the seed actually matters).

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use norn_fixtures::Profile;
use tempfile::TempDir;

/// Walk `root`, returning a sorted map of vault-relative path -> file bytes.
fn walk(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
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
                let bytes = fs::read(&path).unwrap();
                out.insert(rel, bytes);
            }
        }
    }
    out
}

fn generate_into_temp(profile: &Profile, seed: u64) -> (TempDir, BTreeMap<String, Vec<u8>>) {
    let dir = TempDir::new().unwrap();
    // generate() requires an empty (or absent) dir; TempDir::new() already
    // creates an empty directory, so generate directly into it.
    norn_fixtures::generate(profile, seed, dir.path()).unwrap();
    let tree = walk(dir.path());
    (dir, tree)
}

fn assert_same_tree(a: &BTreeMap<String, Vec<u8>>, b: &BTreeMap<String, Vec<u8>>, label: &str) {
    let a_paths: Vec<&String> = a.keys().collect();
    let b_paths: Vec<&String> = b.keys().collect();
    assert_eq!(a_paths, b_paths, "{label}: relative path sets differ");
    for (path, a_bytes) in a {
        let b_bytes = b.get(path).unwrap();
        assert_eq!(a_bytes, b_bytes, "{label}: bytes differ for {path}");
    }
}

#[test]
fn zoo_is_byte_deterministic() {
    let profile_a = Profile::zoo();
    let profile_b = Profile::zoo();
    let (_dir_a, tree_a) = generate_into_temp(&profile_a, 42);
    let (_dir_b, tree_b) = generate_into_temp(&profile_b, 42);
    assert_same_tree(&tree_a, &tree_b, "zoo seed 42");
}

#[test]
fn violations_seed_42_is_byte_deterministic() {
    let profile_a = Profile::violations();
    let profile_b = Profile::violations();
    let (_dir_a, tree_a) = generate_into_temp(&profile_a, 42);
    let (_dir_b, tree_b) = generate_into_temp(&profile_b, 42);
    assert_same_tree(&tree_a, &tree_b, "violations seed 42");
}

#[test]
fn different_seeds_change_at_least_one_file() {
    let profile_a = Profile::violations();
    let profile_b = Profile::violations();
    let (_dir_a, tree_a) = generate_into_temp(&profile_a, 1);
    let (_dir_b, tree_b) = generate_into_temp(&profile_b, 2);

    // Path sets may or may not match (expansion doc placement can shift
    // between seeds); the sanity bar is just "something is different".
    let differs = tree_a != tree_b;
    assert!(
        differs,
        "seed 1 and seed 2 produced byte-identical violations vaults"
    );
}
