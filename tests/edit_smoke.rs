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

// === NRN-210: single-op sugar (ADR 0010 mutate sugar) ===

/// Run `norn edit` with explicit args and no piped stdin (sugar path never
/// reads stdin). Returns the process output.
fn run_edit_args(tmp: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    let mut full = vec!["--cwd", tmp.path().to_str().unwrap(), "edit"];
    full.extend_from_slice(args);
    norn_cmd(tmp)
        .args(&full)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

const SUGAR_SEED: &str =
    "---\ntype: note\n---\nhello world\n\n## Tasks\n\n- one\n\n## Notes\n\nbody\n";

/// The core parity assertion: a sugar invocation and the equivalent
/// `--edits-json` one-op array produce a byte-identical file.
fn assert_sugar_matches_json(sugar: &[&str], json: &str) {
    let tmp_a = fixture();
    let doc_a = tmp_a.path().join("note.md");
    fs::write(&doc_a, SUGAR_SEED).unwrap();
    let out_a = run_edit_args(&tmp_a, &[&["note.md"], sugar, &["--yes"]].concat());
    assert!(
        out_a.status.success(),
        "sugar invocation failed: {}",
        String::from_utf8_lossy(&out_a.stderr)
    );

    let tmp_b = fixture();
    let doc_b = tmp_b.path().join("note.md");
    fs::write(&doc_b, SUGAR_SEED).unwrap();
    let out_b = run_edit_args(&tmp_b, &["note.md", "--edits-json", json, "--yes"]);
    assert!(
        out_b.status.success(),
        "json invocation failed: {}",
        String::from_utf8_lossy(&out_b.stderr)
    );

    assert_eq!(
        fs::read_to_string(&doc_a).unwrap(),
        fs::read_to_string(&doc_b).unwrap(),
        "sugar and --edits-json must produce byte-identical files"
    );
}

#[test]
fn edit_sugar_str_replace_matches_json() {
    assert_sugar_matches_json(
        &["--str-replace", "world", "--new", "norn"],
        r#"[{"op":"str_replace","old":"world","new":"norn"}]"#,
    );
}

#[test]
fn edit_sugar_str_replace_content_alias_matches_new() {
    // `--content` is an accepted alias for `--new` on str_replace.
    assert_sugar_matches_json(
        &["--str-replace", "world", "--content", "norn"],
        r#"[{"op":"str_replace","old":"world","new":"norn"}]"#,
    );
}

#[test]
fn edit_sugar_replace_section_matches_json() {
    assert_sugar_matches_json(
        &["--replace-section", "Tasks", "--content", "rewritten"],
        r#"[{"op":"replace_section","heading":"Tasks","content":"rewritten"}]"#,
    );
}

#[test]
fn edit_sugar_append_to_section_matches_json() {
    assert_sugar_matches_json(
        &["--append-to-section", "Tasks", "--content", "two"],
        r#"[{"op":"append_to_section","heading":"Tasks","content":"two"}]"#,
    );
}

#[test]
fn edit_sugar_delete_section_matches_json() {
    assert_sugar_matches_json(
        &["--delete-section", "Notes"],
        r#"[{"op":"delete_section","heading":"Notes"}]"#,
    );
}

#[test]
fn edit_sugar_insert_before_heading_matches_json() {
    assert_sugar_matches_json(
        &["--insert-before-heading", "Notes", "--content", "intro"],
        r#"[{"op":"insert_before_heading","heading":"Notes","content":"intro"}]"#,
    );
}

#[test]
fn edit_sugar_insert_after_heading_matches_json() {
    assert_sugar_matches_json(
        &["--insert-after-heading", "Notes", "--content", "lead"],
        r#"[{"op":"insert_after_heading","heading":"Notes","content":"lead"}]"#,
    );
}

#[test]
fn edit_sugar_two_op_flags_error() {
    let tmp = fixture();
    fs::write(tmp.path().join("note.md"), SUGAR_SEED).unwrap();
    let out = run_edit_args(
        &tmp,
        &[
            "note.md",
            "--str-replace",
            "world",
            "--new",
            "norn",
            "--delete-section",
            "Notes",
            "--yes",
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "two op flags should hard-error: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn edit_sugar_op_flag_with_edits_json_error() {
    let tmp = fixture();
    fs::write(tmp.path().join("note.md"), SUGAR_SEED).unwrap();
    let out = run_edit_args(
        &tmp,
        &[
            "note.md",
            "--delete-section",
            "Notes",
            "--edits-json",
            r#"[{"op":"delete_section","heading":"Tasks"}]"#,
            "--yes",
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "op flag + --edits-json should hard-error: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn edit_ops_file_reads_from_file() {
    let tmp = fixture();
    let doc = tmp.path().join("note.md");
    fs::write(&doc, SUGAR_SEED).unwrap();
    let ops_path = tmp.path().join("ops.json");
    fs::write(
        &ops_path,
        r#"[{"op":"str_replace","old":"world","new":"norn"}]"#,
    )
    .unwrap();

    let out = run_edit_args(
        &tmp,
        &["note.md", "--ops-file", ops_path.to_str().unwrap(), "--yes"],
    );
    assert!(
        out.status.success(),
        "--ops-file invocation failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(fs::read_to_string(&doc).unwrap().contains("hello norn"));
}
