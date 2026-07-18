//! Byte-determinism: the same `(profile, seed)` must produce a
//! byte-identical vault tree, and a different seed must change at least one
//! byte somewhere (sanity that the seed actually matters).
//!
//! The cases cover both rng-consumption branch families deliberately: `zoo`
//! has `expansion_docs: 0` and pins the static zoo (no rng), while `clean`
//! (non-violating expansion arm) and `violations` (violating expansion arm)
//! each exercise the seeded generator.

mod common;

use std::collections::BTreeMap;

use common::{generate_vault, walk};
use norn_fixtures::Profile;

fn tree(profile: &Profile, seed: u64) -> BTreeMap<String, Vec<u8>> {
    let (_dir, vault, _manifest) = generate_vault(profile, seed);
    walk(&vault)
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
    // Static zoo, no rng consumed — pins the fixed content.
    assert_same_tree(
        &tree(&Profile::zoo(), 42),
        &tree(&Profile::zoo(), 42),
        "zoo seed 42",
    );
}

#[test]
fn clean_seed_7_is_byte_deterministic() {
    // Seeded expansion, non-violating arm.
    assert_same_tree(
        &tree(&Profile::clean(), 7),
        &tree(&Profile::clean(), 7),
        "clean seed 7",
    );
}

#[test]
fn violations_seed_42_is_byte_deterministic() {
    // Seeded expansion, violating arm.
    assert_same_tree(
        &tree(&Profile::violations(), 42),
        &tree(&Profile::violations(), 42),
        "violations seed 42",
    );
}

#[test]
fn different_seeds_change_at_least_one_file() {
    // Path sets may or may not match (expansion doc placement can shift
    // between seeds); the sanity bar is just "something is different".
    let differs = tree(&Profile::violations(), 1) != tree(&Profile::violations(), 2);
    assert!(
        differs,
        "seed 1 and seed 2 produced byte-identical violations vaults"
    );
}
