//! Exit-code and outcome contract for the `norn` binary, driven against the
//! built bin (`env!("CARGO_BIN_EXE_norn")`). These pin the tri-state exit
//! contract (`docs/errors.md`): 0 ok, 1 operational, 2 bad invocation.

use std::process::Command;

fn norn() -> Command {
    Command::new(env!("CARGO_BIN_EXE_norn"))
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
