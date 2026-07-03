//! Integration tests for `norn describe`.

use std::process::Command;
use tempfile::TempDir;

fn synth_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-describe-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    let notes = root.join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    std::fs::write(
        notes.join("a.md"),
        "---\ntype: note\nstatus: active\n---\nbody\n",
    )
    .unwrap();
    std::fs::write(
        notes.join("b.md"),
        "---\ntype: note\nstatus: backlog\n---\nbody\n",
    )
    .unwrap();
    std::fs::write(
        notes.join("c.md"),
        "---\ntype: log\nstatus: backlog\n---\nbody\n",
    )
    .unwrap();
    tmp
}

fn norn_bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.push(format!("norn{}", std::env::consts::EXE_SUFFIX));
    p
}

/// Build a `norn` Command with `XDG_CACHE_HOME`/`XDG_STATE_HOME` isolated to
/// per-test subdirs of the test tempdir, so the binary never reads or sweeps
/// the developer's real cache/state trees.
fn norn_cmd(tmp: &TempDir) -> Command {
    let mut c = Command::new(norn_bin());
    c.env("XDG_CACHE_HOME", tmp.path().join(".xdg-cache"))
        .env("XDG_STATE_HOME", tmp.path().join(".xdg-state"));
    c
}

#[test]
fn describe_without_data_omits_data_key() {
    let tmp = synth_vault();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["describe", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert!(v.get("data").is_none(), "no --data: {v}");
    assert!(v["folders"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("notes")));
}

#[test]
fn describe_data_flag_emits_totals_and_distributions() {
    let tmp = synth_vault();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["describe", "--data", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(v["data"]["total"], 3);
    let fields = v["data"]["fields"].as_array().unwrap();
    assert!(
        fields.iter().any(|f| f["field"] == "type"),
        "expected `type` distribution: {v}"
    );
}

#[test]
fn describe_stats_alias_behaves_like_data() {
    let tmp = synth_vault();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["describe", "--stats", "--format", "json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(v["data"]["total"], 3);
}

#[test]
fn describe_by_implies_data_and_bypasses_identity_skip() {
    let tmp = synth_vault();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["describe", "--by", "status", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    let fields = v["data"]["fields"].as_array().unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0]["field"], "status");
}

#[test]
fn describe_filter_then_data_narrows_totals() {
    let tmp = synth_vault();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args([
            "describe",
            "--eq",
            "type:note",
            "--data",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(v["data"]["total"], 2);
}

#[test]
fn describe_text_format_renders_totals_and_distribution() {
    let tmp = synth_vault();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["describe", "--data"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(stdout.contains("3 documents"), "{stdout}");
    assert!(stdout.contains("type:"), "{stdout}");
}
