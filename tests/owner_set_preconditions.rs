//! Public-interface acceptance tests for MigrationPlan owner-set preconditions.

use std::process::Command;

use tempfile::TempDir;

fn norn_bin() -> std::path::PathBuf {
    let mut path = std::env::current_exe().expect("current test executable");
    path.pop();
    path.pop();
    path.push(format!("norn{}", std::env::consts::EXE_SUFFIX));
    path
}

fn norn_cmd(tmp: &TempDir) -> Command {
    let mut command = Command::new(norn_bin());
    command
        .env("XDG_CACHE_HOME", tmp.path().join(".xdg-cache"))
        .env("XDG_STATE_HOME", tmp.path().join(".xdg-state"));
    command
}

#[test]
fn existing_document_owner_set_mismatch_refuses_before_write() {
    let tmp = tempfile::Builder::new()
        .prefix("norn-owner-set-")
        .tempdir()
        .expect("tempdir");
    let vault = tmp.path().join("vault");
    std::fs::create_dir_all(vault.join("elsewhere")).expect("vault directories");
    let original = "---\ntype: note\nstatus: todo\n---\n# A\n";
    let collider = "---\ntype: note\nstatus: foreign\n---\n# Other A\n";
    std::fs::write(vault.join("a.md"), original).expect("original document");

    // Pin a cache snapshot before the competing owner appears. The owner-set
    // barrier must still consult fresh filesystem state even when the caller
    // explicitly disables the ordinary cache refresh.
    let rebuild = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["cache", "rebuild"])
        .output()
        .expect("rebuild cache");
    assert!(
        rebuild.status.success(),
        "cache rebuild stderr: {}",
        String::from_utf8_lossy(&rebuild.stderr)
    );

    // Client A plans against one logical owner.
    let plan = serde_json::json!({
        "schema_version": 2,
        "vault_root": vault.to_str().expect("UTF-8 vault path"),
        "preconditions": [{
            "id": "a-owner",
            "kind": "owner_set",
            "selector": { "stem": "a" },
            "expected_paths": ["a.md"]
        }],
        "operations": [{
            "id": "update-a",
            "kind": "set_frontmatter",
            "fields": {
                "path": "a.md",
                "field": "status",
                "expected_old_value": "todo",
                "new_value": "done"
            }
        }]
    });
    let plan_path = tmp.path().join("plan.json");
    std::fs::write(
        &plan_path,
        serde_json::to_vec_pretty(&plan).expect("serialize plan"),
    )
    .expect("plan file");

    // Client B wins the planning-to-apply window by introducing another owner.
    std::fs::write(vault.join("elsewhere/a.md"), collider).expect("colliding owner");
    let before_original = std::fs::read(vault.join("a.md")).expect("read original before");
    let before_collider =
        std::fs::read(vault.join("elsewhere/a.md")).expect("read collider before");

    let output = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["apply"])
        .arg(&plan_path)
        .args(["--yes", "--format", "json", "--no-cache-refresh"])
        .output()
        .expect("run norn apply");

    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("structured refusal report");
    assert_eq!(report["schema_version"], 3);
    assert_eq!(report["outcome"], "refused");
    assert_eq!(report["operations"][0]["status"], "not-run");
    assert_eq!(report["preconditions"][0]["id"], "a-owner");
    assert_eq!(report["preconditions"][0]["status"], "failed");
    assert_eq!(
        report["preconditions"][0]["expected_paths"],
        serde_json::json!(["a.md"])
    );
    assert_eq!(
        report["preconditions"][0]["actual_paths"],
        serde_json::json!(["a.md", "elsewhere/a.md"])
    );
    assert_eq!(
        report["preconditions"][0]["error"]["code"],
        "owner-set-mismatch"
    );
    assert_eq!(
        std::fs::read(vault.join("a.md")).expect("read original after"),
        before_original,
        "the planned target must remain byte-identical"
    );
    assert_eq!(
        std::fs::read(vault.join("elsewhere/a.md")).expect("read collider after"),
        before_collider,
        "the colliding owner must remain byte-identical"
    );
}

#[test]
fn resolved_seq_create_owner_set_mismatch_refuses_before_write() {
    let tmp = tempfile::Builder::new()
        .prefix("norn-owner-set-seq-")
        .tempdir()
        .expect("tempdir");
    let vault = tmp.path().join("vault");
    std::fs::create_dir_all(vault.join("items")).expect("items directory");
    std::fs::create_dir_all(vault.join("foreign")).expect("foreign directory");
    std::fs::write(vault.join("items/MMR-1.md"), "# One\n").expect("existing sequence");

    let plan = serde_json::json!({
        "schema_version": 2,
        "vault_root": vault.to_str().expect("UTF-8 vault path"),
        "preconditions": [{
            "id": "new-task-owner",
            "kind": "owner_set",
            "selector": { "stem_from_operation": "create-task" },
            "expected_paths": []
        }],
        "operations": [{
            "id": "create-task",
            "kind": "create_document",
            "fields": {
                "path": "items/MMR-{{seq}}.md",
                "new_value": {
                    "frontmatter": {"type": "task"},
                    "body": "# Task\n"
                }
            }
        }]
    });
    let plan_path = tmp.path().join("plan.json");
    std::fs::write(
        &plan_path,
        serde_json::to_vec_pretty(&plan).expect("serialize plan"),
    )
    .expect("plan file");

    // Client B claims the logical owner that Client A's template resolves to.
    let collider_path = vault.join("foreign/MMR-2.md");
    let collider = "---\ntype: task\n---\n# Foreign two\n";
    std::fs::write(&collider_path, collider).expect("colliding owner");
    let before_collider = std::fs::read(&collider_path).expect("collider before");

    let output = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["apply"])
        .arg(&plan_path)
        .args(["--yes", "--format", "json"])
        .output()
        .expect("run norn apply");

    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("structured refusal report");
    assert_eq!(report["outcome"], "refused");
    assert_eq!(report["operations"][0]["status"], "not-run");
    assert_eq!(
        report["preconditions"][0]["actual_paths"],
        serde_json::json!(["foreign/MMR-2.md"])
    );
    assert_eq!(
        report["preconditions"][0]["error"]["code"],
        "owner-set-mismatch"
    );
    assert!(
        !vault.join("items/MMR-2.md").exists(),
        "the planned create must not run"
    );
    assert_eq!(
        std::fs::read(&collider_path).expect("collider after"),
        before_collider,
        "the colliding owner must remain byte-identical"
    );
}

#[test]
fn project_identity_eq_selector_refuses_duplicate_owner_before_write() {
    let tmp = tempfile::Builder::new()
        .prefix("norn-owner-set-eq-")
        .tempdir()
        .expect("tempdir");
    let vault = tmp.path().join("vault");
    std::fs::create_dir_all(vault.join("projects/archive")).expect("project directories");
    let original = "---\ntype: project\nkey: MMR\ntitle: Mimir\n---\n# Mimir\n";
    std::fs::write(vault.join("projects/mimir.md"), original).expect("original project");

    let plan = serde_json::json!({
        "schema_version": 2,
        "vault_root": vault.to_str().expect("UTF-8 vault path"),
        "preconditions": [{
            "id": "project-owner",
            "kind": "owner_set",
            "selector": { "eq": ["type:project", "key:MMR"] },
            "expected_paths": ["projects/mimir.md"]
        }],
        "operations": [{
            "id": "rename-project",
            "kind": "set_frontmatter",
            "fields": {
                "path": "projects/mimir.md",
                "field": "title",
                "expected_old_value": "Mimir",
                "new_value": "Mimir tracker"
            }
        }]
    });
    let plan_path = tmp.path().join("plan.json");
    std::fs::write(
        &plan_path,
        serde_json::to_vec_pretty(&plan).expect("serialize plan"),
    )
    .expect("plan file");

    let duplicate = "---\ntype: project\nkey: MMR\ntitle: Archived Mimir\n---\n# Old\n";
    std::fs::write(vault.join("projects/archive/old.md"), duplicate).expect("duplicate owner");
    let before_original = std::fs::read(vault.join("projects/mimir.md")).expect("before original");

    let output = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["apply"])
        .arg(&plan_path)
        .args(["--yes", "--format", "json"])
        .output()
        .expect("run norn apply");

    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("structured refusal report");
    assert_eq!(
        report["preconditions"][0]["actual_paths"],
        serde_json::json!(["projects/archive/old.md", "projects/mimir.md"])
    );
    assert_eq!(
        report["preconditions"][0]["error"]["code"],
        "owner-set-mismatch"
    );
    assert_eq!(
        std::fs::read(vault.join("projects/mimir.md")).expect("after original"),
        before_original
    );
}

#[test]
fn conflicting_create_claims_in_one_plan_refuse_before_write() {
    let tmp = tempfile::Builder::new()
        .prefix("norn-owner-set-plan-conflict-")
        .tempdir()
        .expect("tempdir");
    let vault = tmp.path().join("vault");
    std::fs::create_dir_all(vault.join("one")).expect("first directory");
    std::fs::create_dir_all(vault.join("two")).expect("second directory");

    let plan = serde_json::json!({
        "schema_version": 2,
        "vault_root": vault.to_str().expect("UTF-8 vault path"),
        "preconditions": [
            {
                "id": "first-owner",
                "kind": "owner_set",
                "selector": { "stem_from_operation": "create-first" },
                "expected_paths": []
            },
            {
                "id": "second-owner",
                "kind": "owner_set",
                "selector": { "stem_from_operation": "create-second" },
                "expected_paths": []
            }
        ],
        "operations": [
            {
                "id": "create-first",
                "kind": "create_document",
                "fields": {
                    "path": "one/MMR-1.md",
                    "new_value": {"frontmatter": {"type": "task"}, "body": "# First\n"}
                }
            },
            {
                "id": "create-second",
                "kind": "create_document",
                "fields": {
                    "path": "two/MMR-1.md",
                    "new_value": {"frontmatter": {"type": "task"}, "body": "# Second\n"}
                }
            }
        ]
    });
    let plan_path = tmp.path().join("plan.json");
    std::fs::write(
        &plan_path,
        serde_json::to_vec_pretty(&plan).expect("serialize plan"),
    )
    .expect("plan file");

    let output = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["apply"])
        .arg(&plan_path)
        .args(["--yes", "--format", "json"])
        .output()
        .expect("run norn apply");

    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("structured refusal report");
    assert_eq!(report["outcome"], "refused");
    assert_eq!(report["operations"][0]["status"], "not-run");
    assert_eq!(report["operations"][1]["status"], "not-run");
    assert_eq!(report["preconditions"][0]["status"], "failed");
    assert_eq!(report["preconditions"][1]["status"], "failed");
    assert_eq!(
        report["preconditions"][0]["error"]["code"],
        "owner-claim-conflict"
    );
    assert!(
        !vault.join("one/MMR-1.md").exists() && !vault.join("two/MMR-1.md").exists(),
        "neither conflicting claim may write"
    );
}

#[test]
fn all_preconditions_form_one_barrier_before_any_operation() {
    let tmp = tempfile::Builder::new()
        .prefix("norn-owner-set-barrier-")
        .tempdir()
        .expect("tempdir");
    let vault = tmp.path().join("vault");
    std::fs::create_dir_all(vault.join("foreign")).expect("foreign directory");
    let a = "---\nstatus: todo\n---\n# A\n";
    let b = "---\nstatus: todo\n---\n# B\n";
    std::fs::write(vault.join("a.md"), a).expect("a");
    std::fs::write(vault.join("b.md"), b).expect("b");
    std::fs::write(vault.join("foreign/b.md"), "# Other B\n").expect("duplicate b");

    let plan = serde_json::json!({
        "schema_version": 2,
        "vault_root": vault.to_str().expect("UTF-8 vault path"),
        "preconditions": [
            {
                "id": "a-owner",
                "kind": "owner_set",
                "selector": {"stem": "a"},
                "expected_paths": ["a.md"]
            },
            {
                "id": "b-owner",
                "kind": "owner_set",
                "selector": {"stem": "b"},
                "expected_paths": ["b.md"]
            }
        ],
        "operations": [
            {
                "kind": "set_frontmatter",
                "fields": {
                    "path": "a.md", "field": "status",
                    "expected_old_value": "todo", "new_value": "done"
                }
            },
            {
                "kind": "set_frontmatter",
                "fields": {
                    "path": "b.md", "field": "status",
                    "expected_old_value": "todo", "new_value": "done"
                }
            }
        ]
    });
    let plan_path = tmp.path().join("plan.json");
    std::fs::write(&plan_path, serde_json::to_vec_pretty(&plan).unwrap()).expect("plan");

    let output = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["apply"])
        .arg(&plan_path)
        .args(["--yes", "--format", "json"])
        .output()
        .expect("run norn apply");

    assert_eq!(output.status.code(), Some(2));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["preconditions"][0]["status"], "passed");
    assert_eq!(report["preconditions"][1]["status"], "failed");
    assert!(report["operations"]
        .as_array()
        .unwrap()
        .iter()
        .all(|operation| operation["status"] == "not-run"));
    assert_eq!(std::fs::read(vault.join("a.md")).unwrap(), a.as_bytes());
    assert_eq!(std::fs::read(vault.join("b.md")).unwrap(), b.as_bytes());
}

#[test]
fn successful_apply_reports_v3_passed_preconditions() {
    let tmp = tempfile::Builder::new()
        .prefix("norn-owner-set-success-")
        .tempdir()
        .expect("tempdir");
    let vault = tmp.path().join("vault");
    std::fs::create_dir_all(&vault).expect("vault");
    std::fs::write(vault.join("a.md"), "---\nstatus: todo\n---\n# A\n").expect("a");

    let plan = serde_json::json!({
        "schema_version": 2,
        "vault_root": vault.to_str().expect("UTF-8 vault path"),
        "preconditions": [{
            "id": "a-owner",
            "kind": "owner_set",
            "selector": {"stem": "a"},
            "expected_paths": ["a.md"]
        }],
        "operations": [{
            "kind": "set_frontmatter",
            "fields": {
                "path": "a.md", "field": "status",
                "expected_old_value": "todo", "new_value": "done"
            }
        }]
    });
    let plan_path = tmp.path().join("plan.json");
    std::fs::write(&plan_path, serde_json::to_vec_pretty(&plan).unwrap()).expect("plan");

    let output = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["apply"])
        .arg(&plan_path)
        .args(["--yes", "--format", "json"])
        .output()
        .expect("run norn apply");

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema_version"], 3);
    assert_eq!(report["outcome"], "applied");
    assert_eq!(report["preconditions"][0]["status"], "passed");
    assert_eq!(
        report["preconditions"][0]["expected_paths"],
        serde_json::json!(["a.md"])
    );
    assert_eq!(
        report["preconditions"][0]["actual_paths"],
        serde_json::json!(["a.md"])
    );
    assert_eq!(report["operations"][0]["status"], "applied");
    assert!(std::fs::read_to_string(vault.join("a.md"))
        .unwrap()
        .contains("status: done"));
}
