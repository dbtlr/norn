//! Inventory checks: `zoo` carries the representative fixed files (valid
//! and violation zoo alike); `clean` carries the valid zoo plus its
//! expansion docs, and has no violation-zoo files at all.

use std::path::Path;

use norn_fixtures::Profile;
use tempfile::TempDir;

fn generate(profile: &Profile, seed: u64) -> TempDir {
    let dir = TempDir::new().unwrap();
    norn_fixtures::generate(profile, seed, dir.path()).unwrap();
    dir
}

fn count_md_files(root: &Path) -> usize {
    let mut count = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                count += 1;
            }
        }
    }
    count
}

#[test]
fn zoo_contains_representative_sample() {
    let dir = generate(&Profile::zoo(), 1);
    let root = dir.path();

    for rel in [
        ".norn/config.yaml",
        ".norn-fixture-vault",
        "notes/alpha.md",
        "Über Notiz.md",
        "Assets/pic.png",
        "templates/broken-template.md",
        "broken/parse-fail.md",
    ] {
        assert!(
            root.join(rel).exists(),
            "expected {rel} to exist in the zoo profile"
        );
    }
}

#[test]
fn clean_has_no_violation_zoo_and_has_expansion_docs() {
    let profile = Profile::clean();
    let expansion_docs = profile.expansion_docs;
    let dir = generate(&profile, 1);
    let root = dir.path();

    // Violation-zoo-only files must be absent.
    for rel in [
        "broken/parse-fail.md",
        "broken/no-frontmatter.md",
        "notes/missing-kind.md",
        "notes/has-legacy.md",
        "tasks/task-bad-status.md",
        "stray-task.md",
        "notes/bad-types.md",
        "notes/bad-parent.md",
        "notes/dangling-parent.md",
        "notes/ambi-bare.md",
        "notes/dead-end.md",
        "notes/into-ignored.md",
        "shapes/empty-block.md",
    ] {
        assert!(
            !root.join(rel).exists(),
            "expected {rel} to be absent from the clean profile"
        );
    }

    // Valid zoo files are still present.
    assert!(root.join("notes/alpha.md").exists());
    assert!(root.join(".norn/config.yaml").exists());

    // Exactly the fixed valid zoo (23 docs, see crate::zoo::valid_docs) plus
    // `expansion_docs` seeded docs.
    let total_md = count_md_files(root);
    let valid_zoo_doc_count = 23;
    assert_eq!(
        total_md,
        valid_zoo_doc_count + expansion_docs,
        "expected exactly {} markdown docs (zoo + expansion), found {total_md}",
        valid_zoo_doc_count + expansion_docs
    );
}
