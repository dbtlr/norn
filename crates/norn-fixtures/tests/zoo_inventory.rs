//! Inventory checks: `zoo` carries the representative fixed files (valid
//! and violation zoo alike); `clean` carries the valid zoo plus its
//! expansion docs, and has no violation-zoo files at all. The valid-zoo doc
//! count is derived from the generated manifest rather than hardcoded.

mod common;

use common::{count_md_files, generate_vault};
use norn_fixtures::{Profile, Tier};

#[test]
fn zoo_contains_representative_sample() {
    let (_dir, vault, _manifest) = generate_vault(&Profile::zoo(), 1);

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
            vault.join(rel).exists(),
            "expected {rel} to exist in the zoo profile"
        );
    }
}

#[test]
fn clean_has_no_violation_zoo_and_has_expansion_docs() {
    // The valid-zoo doc count is whatever the zoo manifest reports as
    // non-violation docs — derived, not hardcoded — with a single literal
    // spot-check so a manifest bug can't self-certify.
    let (_zdir, _zvault, zoo_manifest) = generate_vault(&Profile::zoo(), 1);
    let valid_zoo_doc_count = zoo_manifest
        .docs
        .iter()
        .filter(|d| d.tier != Tier::Violation)
        .count();
    assert_eq!(
        valid_zoo_doc_count, 23,
        "valid-zoo doc count drifted from the expected 23"
    );

    let profile = Profile::clean();
    let expansion_docs = profile.expansion_docs;
    let (_dir, vault, _manifest) = generate_vault(&profile, 1);

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
            !vault.join(rel).exists(),
            "expected {rel} to be absent from the clean profile"
        );
    }

    // Valid zoo files are still present.
    assert!(vault.join("notes/alpha.md").exists());
    assert!(vault.join(".norn/config.yaml").exists());

    // Exactly the manifest-derived valid zoo plus `expansion_docs` seeded docs.
    let total_md = count_md_files(&vault);
    assert_eq!(
        total_md,
        valid_zoo_doc_count + expansion_docs,
        "expected exactly {} markdown docs (zoo + expansion), found {total_md}",
        valid_zoo_doc_count + expansion_docs
    );
}
