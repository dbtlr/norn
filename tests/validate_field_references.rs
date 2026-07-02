//! Integration coverage for the typed-reference constraint (NRN-73):
//! `field_references: { field: { target_type: [...] } }` emits a
//! `frontmatter-reference-type` finding when a frontmatter wikilink points
//! at a document whose `type` is outside the allowed set.

use std::fs;
use std::process::Command;

use tempfile::TempDir;

fn isolate_cache(command: &mut Command) -> TempDir {
    let dir = tempfile::tempdir().expect("temp cache dir should be created");
    command.env("XDG_CACHE_HOME", dir.path());
    command.env("XDG_STATE_HOME", dir.path().join("state"));
    dir
}

/// Vault fixture: a Mimir-shaped node tree.
///   phase-1.md      — type: phase
///   note-1.md       — type: note
///   untyped.md      — frontmatter without `type`
///   task-good.md    — type: task, parent → [[phase-1]]        (legal)
///   task-bad.md     — type: task, parent → [[note-1]]         (wrong type)
///   task-untyped.md — type: task, parent → [[untyped]]        (target type missing)
///   task-ghost.md   — type: task, parent → [[ghost]]          (unresolved: link-*'s job)
///   task-multi.md   — type: task, depends_on → [[task-good]], [[note-1]]
fn build_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-fieldrefs-")
        .tempdir()
        .unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join(".norn")).unwrap();
    fs::write(
        root.join(".norn/config.yaml"),
        concat!(
            "validate:\n",
            "  rules:\n",
            "    - name: task-refs\n",
            "      match:\n",
            "        frontmatter:\n",
            "          type: task\n",
            "      field_references:\n",
            "        parent:\n",
            "          target_type: [phase, initiative]\n",
            "        depends_on:\n",
            "          target_type: task\n",
        ),
    )
    .unwrap();
    fs::write(root.join("phase-1.md"), "---\ntype: phase\n---\n").unwrap();
    fs::write(root.join("note-1.md"), "---\ntype: note\n---\n").unwrap();
    fs::write(root.join("untyped.md"), "---\ntitle: no type\n---\n").unwrap();
    fs::write(
        root.join("task-good.md"),
        "---\ntype: task\nparent: \"[[phase-1]]\"\n---\n",
    )
    .unwrap();
    fs::write(
        root.join("task-bad.md"),
        "---\ntype: task\nparent: \"[[note-1]]\"\n---\n",
    )
    .unwrap();
    fs::write(
        root.join("task-untyped.md"),
        "---\ntype: task\nparent: \"[[untyped]]\"\n---\n",
    )
    .unwrap();
    fs::write(
        root.join("task-ghost.md"),
        "---\ntype: task\nparent: \"[[ghost]]\"\n---\n",
    )
    .unwrap();
    fs::write(
        root.join("task-multi.md"),
        "---\ntype: task\ndepends_on:\n  - \"[[task-good]]\"\n  - \"[[note-1]]\"\n---\n",
    )
    .unwrap();
    tmp
}

fn validate_findings(vault: &TempDir) -> Vec<serde_json::Value> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.arg("-C").arg(vault.path());
    command.args(["validate", "--format", "json"]);
    let _cache = isolate_cache(&mut command);
    let output = command.output().expect("norn validate should run");
    assert!(
        output.status.success(),
        "validate failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("output should be JSON");
    parsed["findings"].as_array().unwrap().clone()
}

fn reference_findings(findings: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    findings
        .iter()
        .filter(|f| f["code"] == "frontmatter-reference-type")
        .collect()
}

#[test]
fn wrong_target_type_emits_reference_finding() {
    let vault = build_vault();
    let findings = validate_findings(&vault);
    let refs = reference_findings(&findings);

    let bad = refs
        .iter()
        .find(|f| f["path"] == "task-bad.md")
        .unwrap_or_else(|| panic!("expected finding for task-bad.md, got: {refs:?}"));
    assert_eq!(bad["field"], "parent");
    assert_eq!(bad["target"], "note-1.md");
    assert_eq!(bad["actual_type"], "note");
    assert_eq!(
        bad["allowed_types"],
        serde_json::json!(["phase", "initiative"])
    );
    assert_eq!(bad["rule"], "task-refs");
}

#[test]
fn legal_target_type_emits_nothing() {
    let vault = build_vault();
    let findings = validate_findings(&vault);
    assert!(
        !reference_findings(&findings)
            .iter()
            .any(|f| f["path"] == "task-good.md"),
        "task-good.md parent points at a phase; no finding expected"
    );
}

#[test]
fn target_without_type_counts_as_outside_the_set() {
    let vault = build_vault();
    let findings = validate_findings(&vault);
    let refs = reference_findings(&findings);
    let untyped = refs
        .iter()
        .find(|f| f["path"] == "task-untyped.md")
        .unwrap_or_else(|| panic!("expected finding for task-untyped.md, got: {refs:?}"));
    assert_eq!(untyped["actual_type"], "(missing)");
}

#[test]
fn unresolved_reference_is_links_job_not_ours() {
    let vault = build_vault();
    let findings = validate_findings(&vault);
    assert!(
        !reference_findings(&findings)
            .iter()
            .any(|f| f["path"] == "task-ghost.md"),
        "unresolved targets are covered by link-* codes, not reference-type"
    );
    assert!(
        findings
            .iter()
            .any(|f| f["code"] == "link-target-missing" && f["path"] == "task-ghost.md"),
        "the broken link itself still surfaces via link validation"
    );
}

#[test]
fn array_valued_field_checks_each_element() {
    let vault = build_vault();
    let findings = validate_findings(&vault);
    let refs = reference_findings(&findings);
    let multi: Vec<_> = refs
        .iter()
        .filter(|f| f["path"] == "task-multi.md")
        .collect();
    assert_eq!(
        multi.len(),
        1,
        "only the note-1 element violates depends_on target_type: {multi:?}"
    );
    assert_eq!(multi[0]["target"], "note-1.md");
    assert_eq!(multi[0]["allowed_types"], serde_json::json!(["task"]));
}

#[test]
fn scalar_target_type_is_a_one_element_set() {
    // depends_on's constraint is written as a scalar (`target_type: task`);
    // it must behave exactly like `[task]`.
    let vault = build_vault();
    let findings = validate_findings(&vault);
    assert!(
        !reference_findings(&findings)
            .iter()
            .any(|f| f["path"] == "task-multi.md" && f["target"] == "task-good.md"),
        "task-good.md is type task; the scalar constraint allows it"
    );
}
