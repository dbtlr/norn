//! Exit-code and outcome contract for the `norn` binary, driven against the
//! built bin (`env!("CARGO_BIN_EXE_norn")`). These pin the tri-state exit
//! contract (`docs/errors.md`): 0 ok, 1 operational, 2 bad invocation.

use std::path::Path;
use std::process::Command;

fn norn() -> Command {
    Command::new(env!("CARGO_BIN_EXE_norn"))
}

/// A `norn` invocation with an isolated central-config home, so the registry
/// end-to-end tests never touch the developer's real `~/.config/norn`.
fn norn_cfg(config_dir: &Path) -> Command {
    let mut cmd = norn();
    cmd.env("NORN_CONFIG_DIR", config_dir);
    // Neutralize any ambient overrides that would otherwise steer from_env.
    cmd.env_remove("XDG_CONFIG_HOME");
    cmd
}

fn stdout_of(out: &std::process::Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn stderr_of(out: &std::process::Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

#[test]
fn version_exits_zero_and_prints_name_and_version() {
    let out = norn().arg("--version").output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    // `norn <version>` — workspace placeholder 0.0.0 on the rewrite branch.
    assert!(stdout.starts_with("norn "), "got: {stdout:?}");
    assert!(stdout.trim_end().ends_with("0.0.0"), "got: {stdout:?}");
}

#[test]
fn help_exits_zero() {
    let out = norn().arg("--help").output().unwrap();
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn unknown_command_exits_two() {
    let out = norn().arg("definitely-not-a-command").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn bad_flag_exits_two() {
    let out = norn().args(["find", "--nope"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn get_missing_required_target_exits_two() {
    let out = norn().arg("get").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn find_unported_exits_one_with_uniform_line() {
    let out = norn().arg("find").output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "stdout must stay empty");
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        "norn: `find` is not yet ported in this build (rewrite in progress; see ADR 0018)\n"
    );
}

#[test]
fn get_unported_exits_one_with_uniform_line() {
    let out = norn().args(["get", "alpha"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        "norn: `get` is not yet ported in this build (rewrite in progress; see ADR 0018)\n"
    );
}

// The registry verb surface (NRN-328) — the one namespace that EXECUTES.
// Driven end-to-end against an isolated `NORN_CONFIG_DIR`.

#[test]
fn vault_register_list_set_unregister_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = tmp.path().join("cfg");
    let docs = tmp.path().join("docs");
    let notes = tmp.path().join("notes");
    let newroot = tmp.path().join("newroot");
    for dir in [&docs, &notes, &newroot] {
        std::fs::create_dir_all(dir).unwrap();
    }

    // register docs (explicit path) — confirmation to stdout, exit 0.
    let out = norn_cfg(&cfg)
        .args(["vault", "register", "docs"])
        .arg(&docs)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(
        stdout_of(&out).starts_with("norn: registered \"docs\" -> "),
        "got: {:?}",
        stdout_of(&out)
    );

    // register notes with a cache override.
    let out = norn_cfg(&cfg)
        .args(["vault", "register", "notes"])
        .arg(&notes)
        .args(["--cache", "/tmp/notes-cache"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));

    // list human — sorted, one row per vault, override indented.
    let out = norn_cfg(&cfg).args(["vault", "list"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let human = stdout_of(&out);
    let docs_line = human
        .lines()
        .position(|l| l.starts_with("docs  "))
        .unwrap_or_else(|| panic!("docs row missing: {human}"));
    let notes_line = human
        .lines()
        .position(|l| l.starts_with("notes  "))
        .unwrap_or_else(|| panic!("notes row missing: {human}"));
    assert!(docs_line < notes_line, "not name-sorted: {human}");
    assert!(
        human.contains("    cache = /tmp/notes-cache"),
        "override not shown: {human}"
    );

    // list json — stable shape, absent overrides are null.
    let out = norn_cfg(&cfg)
        .args(["vault", "list", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let json: serde_json::Value = serde_json::from_str(&stdout_of(&out)).unwrap();
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["name"], "docs");
    assert_eq!(arr[0]["cache"], serde_json::Value::Null);
    assert_eq!(arr[1]["name"], "notes");
    assert_eq!(arr[1]["cache"], "/tmp/notes-cache");

    // set docs: move its cache, then re-point its root.
    let out = norn_cfg(&cfg)
        .args(["vault", "set", "docs", "--cache", "/tmp/c1"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout_of(&out).starts_with("norn: updated \"docs\" -> "));

    let out = norn_cfg(&cfg)
        .args(["vault", "set", "docs", "--root"])
        .arg(&newroot)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));

    // set notes: clear its cache override.
    let out = norn_cfg(&cfg)
        .args(["vault", "set", "notes", "--clear-cache"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));

    // Verify the mutations landed via json.
    let out = norn_cfg(&cfg)
        .args(["vault", "list", "--format", "json"])
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout_of(&out)).unwrap();
    let arr = json.as_array().unwrap();
    assert_eq!(arr[0]["cache"], "/tmp/c1");
    assert!(arr[0]["root"].as_str().unwrap().ends_with("newroot"));
    assert_eq!(arr[1]["cache"], serde_json::Value::Null);

    // The config file carries the managed-file banner.
    let text = std::fs::read_to_string(cfg.join("config.toml")).unwrap();
    assert!(text.starts_with("# Managed by norn"), "no banner: {text}");

    // unregister docs — exit 0, gone from the listing.
    let out = norn_cfg(&cfg)
        .args(["vault", "unregister", "docs"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout_of(&out), "norn: unregistered \"docs\"\n");

    let out = norn_cfg(&cfg)
        .args(["vault", "list", "--format", "json"])
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout_of(&out)).unwrap();
    assert_eq!(json.as_array().unwrap().len(), 1);
}

#[test]
fn vault_list_empty_is_a_stderr_note_exit_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let out = norn_cfg(&tmp.path().join("cfg"))
        .args(["vault", "list"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty(), "stdout must stay empty");
    assert_eq!(stderr_of(&out), "norn: no vaults registered\n");
}

#[test]
fn vault_register_duplicate_name_exits_one() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = tmp.path().join("cfg");
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    norn_cfg(&cfg)
        .args(["vault", "register", "docs"])
        .arg(&a)
        .output()
        .unwrap();
    let out = norn_cfg(&cfg)
        .args(["vault", "register", "docs"])
        .arg(&b)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr_of(&out),
        "norn: a vault named \"docs\" is already registered\n"
    );
}

#[test]
fn vault_set_unknown_name_exits_one() {
    let tmp = tempfile::tempdir().unwrap();
    let out = norn_cfg(&tmp.path().join("cfg"))
        .args(["vault", "set", "ghost", "--clear-cache"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr_of(&out),
        "norn: no vault named \"ghost\" is registered\n"
    );
}

#[test]
fn vault_register_missing_root_exits_one() {
    let tmp = tempfile::tempdir().unwrap();
    let out = norn_cfg(&tmp.path().join("cfg"))
        .args(["vault", "register", "docs"])
        .arg(tmp.path().join("does-not-exist"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr_of(&out).starts_with("norn: failed to canonicalize vault root"),
        "got: {:?}",
        stderr_of(&out)
    );
}

#[test]
fn vault_paths_honor_global_cwd() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = tmp.path().join("cfg");
    let base = tmp.path().join("base");
    let vault_a = base.join("vault-a");
    let vault_b = base.join("vault-b");
    std::fs::create_dir_all(&vault_a).unwrap();
    std::fs::create_dir_all(&vault_b).unwrap();

    // register with no PATH: -C is the effective cwd, so vault-a is the root.
    let out = norn_cfg(&cfg)
        .arg("-C")
        .arg(&vault_a)
        .args(["vault", "register", "docs"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    let canon_a = vault_a.canonicalize().unwrap();
    assert!(
        stdout_of(&out).contains(&canon_a.display().to_string()),
        "-C not honored as register's default PATH: {}",
        stdout_of(&out)
    );

    // set --root with a RELATIVE path: grounded against -C, not process cwd.
    let out = norn_cfg(&cfg)
        .arg("-C")
        .arg(&base)
        .args(["vault", "set", "docs", "--root", "vault-b"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    let canon_b = vault_b.canonicalize().unwrap();
    assert!(
        stdout_of(&out).contains(&canon_b.display().to_string()),
        "relative --root not grounded against -C: {}",
        stdout_of(&out)
    );
}

#[test]
fn vault_set_noop_reports_no_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = tmp.path().join("cfg");
    let vault = tmp.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    let out = norn_cfg(&cfg)
        .args(["vault", "register", "docs"])
        .arg(&vault)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));

    // Clearing an override that was never set changes nothing.
    let out = norn_cfg(&cfg)
        .args(["vault", "set", "docs", "--clear-cache"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout_of(&out), "norn: no changes for \"docs\"\n");
    assert_eq!(stderr_of(&out), "");
}

#[test]
fn vault_set_with_no_change_flags_exits_two() {
    let tmp = tempfile::tempdir().unwrap();
    let out = norn_cfg(&tmp.path().join("cfg"))
        .args(["vault", "set", "docs"])
        .output()
        .unwrap();
    // Empty change set is a usage error, decided by clap before dispatch.
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn vault_relative_config_dir_fails_loud() {
    // A relative NORN_CONFIG_DIR must fail loud (exit 1), never depend on cwd.
    let out = norn()
        .env("NORN_CONFIG_DIR", "relative/dir")
        .args(["vault", "list"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr_of(&out).contains("NORN_CONFIG_DIR must be an absolute path"),
        "got: {:?}",
        stderr_of(&out)
    );
}
