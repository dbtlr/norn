//! Process-level integration tests for `norn edit`.
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};
use tempfile::Builder;

fn norn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_norn")
}

fn norn_cmd(tmp: &tempfile::TempDir) -> Command {
    let mut c = Command::new(norn_bin());
    c.env("XDG_CACHE_HOME", tmp.path().join(".xdg-cache"))
        .env("XDG_STATE_HOME", tmp.path().join(".xdg-state"));
    c
}

fn fixture() -> tempfile::TempDir {
    let tmp = Builder::new().prefix("norn-edit-").tempdir().unwrap();
    fs::create_dir_all(tmp.path().join(".norn")).unwrap();
    fs::write(tmp.path().join(".norn/config.yaml"), "validate: {}\n").unwrap();
    tmp
}

fn run_edit(
    tmp: &tempfile::TempDir,
    doc: &str,
    edits: &str,
    extra: &[&str],
) -> std::process::Output {
    let mut args = vec!["--cwd", tmp.path().to_str().unwrap(), "edit", doc];
    args.extend_from_slice(extra);
    let mut child = norn_cmd(tmp)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(edits.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn edit_str_replace_applies_via_stdin() {
    let tmp = fixture();
    let doc = tmp.path().join("note.md");
    fs::write(&doc, "---\ntype: note\n---\nhello world\n").unwrap();

    let out = run_edit(
        &tmp,
        "note.md",
        r#"[{"op":"str_replace","old":"world","new":"norn"}]"#,
        &["--yes", "--format", "json"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let written = fs::read_to_string(&doc).unwrap();
    assert!(written.contains("hello norn"), "got: {written}");
    assert!(
        written.contains("type: note"),
        "frontmatter preserved: {written}"
    );

    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["operation"], "edit");
    assert_eq!(v["applied"], true);
    assert_eq!(v["edits"][0]["op"], "str_replace");
}

#[test]
fn edit_dry_run_writes_nothing() {
    let tmp = fixture();
    let doc = tmp.path().join("note.md");
    fs::write(&doc, "---\ntype: note\n---\nhello world\n").unwrap();

    let out = run_edit(
        &tmp,
        "note.md",
        r#"[{"op":"str_replace","old":"world","new":"norn"}]"#,
        &["--dry-run", "--format", "json"],
    );
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["applied"], false);
    assert_eq!(
        fs::read_to_string(&doc).unwrap(),
        "---\ntype: note\n---\nhello world\n"
    );
}

#[test]
fn edit_ambiguous_str_refuses_exit_2() {
    let tmp = fixture();
    let doc = tmp.path().join("note.md");
    fs::write(&doc, "---\ntype: note\n---\na a a\n").unwrap();

    let out = run_edit(
        &tmp,
        "note.md",
        r#"[{"op":"str_replace","old":"a","new":"b"}]"#,
        &["--yes", "--format", "json"],
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("matched 3 times"));
    // Unchanged on refusal.
    assert!(fs::read_to_string(&doc).unwrap().contains("a a a"));
}
