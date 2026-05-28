//! Integration tests: every intent-verb's `--help` output includes a PLAN OPERATION block.
//!
//! The PLAN OPERATION block teaches users (and agents) the exact YAML they
//! would put in a MigrationPlan to invoke the same operation as the verb.
//! Block is rendered via the `conceptual_sections` path in the custom help
//! renderer, so it appears only on `--help` (long form), not `-h`.

use std::process::Command;

fn norn_long_help(verb: &str) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_norn"));
    cmd.args([verb, "--help"]);
    cmd.env("NO_COLOR", "1");
    let out = cmd.output().expect("norn should run");
    assert!(
        out.status.success(),
        "norn {} --help should exit 0\nstderr: {}",
        verb,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("stdout should be UTF-8")
}

fn assert_plan_operation_block(verb: &str, expected_kind: &str) {
    let stdout = norn_long_help(verb);
    assert!(
        stdout.contains("PLAN OPERATION"),
        "{} --help should contain PLAN OPERATION block\nGOT:\n{}",
        verb,
        stdout
    );
    assert!(
        stdout.contains(&format!("kind: {}", expected_kind)),
        "{} --help should reference op kind '{}'\nGOT:\n{}",
        verb,
        expected_kind,
        stdout
    );
}

#[test]
fn move_help_has_plan_operation() {
    assert_plan_operation_block("move", "move_document");
}

#[test]
fn delete_help_has_plan_operation() {
    assert_plan_operation_block("delete", "delete_document");
}

#[test]
fn rewrite_wikilink_help_has_plan_operation() {
    assert_plan_operation_block("rewrite-wikilink", "rewrite_wikilink");
}

#[test]
fn set_help_has_plan_operation() {
    assert_plan_operation_block("set", "set_frontmatter");
}

#[test]
fn new_help_has_plan_operation() {
    assert_plan_operation_block("new", "create_document");
}
