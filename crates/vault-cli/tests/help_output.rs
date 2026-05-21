//! Integration tests for the custom help renderer.
//!
//! Tests invoke the real binary so they cover the full path: clap parse →
//! intercept → build_model → render → write. Assertions check structural
//! properties (sections present, headings uppercase, globals shown, footer
//! present) rather than exact bytes, so cosmetic edits don't break them.

use std::process::Command;

fn vault_help(args: &[&str]) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vault"));
    command.args(args);
    // NO_COLOR strips ANSI so assertions match against plain text.
    command.env("NO_COLOR", "1");
    let output = command.output().expect("vault command should run");
    assert!(
        output.status.success(),
        "vault {} failed\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("stdout should be UTF-8")
}

/// Every command answers `-h` with a USAGE block, at least one group heading,
/// a GLOBAL OPTIONS block, and a long-form pointer footer.
fn assert_short_help_shape(out: &str, cmd_path: &str) {
    assert!(
        out.contains("USAGE\n"),
        "{cmd_path} -h missing USAGE block:\n{out}"
    );
    assert!(
        out.contains(&format!("{cmd_path} [OPTIONS]")),
        "{cmd_path} -h missing usage line:\n{out}"
    );
    assert!(
        out.contains("GLOBAL OPTIONS\n"),
        "{cmd_path} -h missing GLOBAL OPTIONS:\n{out}"
    );
    assert!(
        out.contains(&format!("For full help, run `{cmd_path} --help`.")),
        "{cmd_path} -h missing long-form pointer:\n{out}"
    );
}

/// Every command answers `--help` with a USAGE block, GLOBAL OPTIONS, and a
/// docs footer.
fn assert_long_help_shape(out: &str, cmd_path: &str) {
    assert!(
        out.contains("USAGE\n"),
        "{cmd_path} --help missing USAGE:\n{out}"
    );
    assert!(
        out.contains("GLOBAL OPTIONS\n"),
        "{cmd_path} --help missing GLOBAL OPTIONS:\n{out}"
    );
    assert!(
        out.to_lowercase().contains("documentation") || out.contains("github.com"),
        "{cmd_path} --help missing docs footer:\n{out}"
    );
}

#[test]
fn root_short_help() {
    let out = vault_help(&["-h"]);
    assert_short_help_shape(&out, "vault");
    // Root help should list subcommands.
    assert!(
        out.contains("COMMANDS\n"),
        "root -h should list COMMANDS:\n{out}"
    );
    assert!(out.contains("find"));
    assert!(out.contains("repair"));
}

#[test]
fn root_long_help() {
    let out = vault_help(&["--help"]);
    assert_long_help_shape(&out, "vault");
}

#[test]
fn find_short_help() {
    let out = vault_help(&["find", "-h"]);
    assert_short_help_shape(&out, "vault find");
    assert!(out.contains("FILTER")); // some filtering-flavored group exists
}

#[test]
fn find_long_help() {
    let out = vault_help(&["find", "--help"]);
    assert_long_help_shape(&out, "vault find");
}

#[test]
fn init_short_help() {
    let out = vault_help(&["init", "-h"]);
    assert_short_help_shape(&out, "vault init");
}

#[test]
fn init_long_help() {
    let out = vault_help(&["init", "--help"]);
    assert_long_help_shape(&out, "vault init");
}

#[test]
fn validate_short_help() {
    let out = vault_help(&["validate", "-h"]);
    assert_short_help_shape(&out, "vault validate");
}

#[test]
fn validate_long_help() {
    let out = vault_help(&["validate", "--help"]);
    assert_long_help_shape(&out, "vault validate");
}

#[test]
fn repair_plan_short_help() {
    let out = vault_help(&["repair", "plan", "-h"]);
    assert_short_help_shape(&out, "vault repair plan");
}

#[test]
fn repair_apply_long_help() {
    let out = vault_help(&["repair", "apply", "--help"]);
    assert_long_help_shape(&out, "vault repair apply");
}

#[test]
fn repair_links_short_help() {
    let out = vault_help(&["repair", "links", "-h"]);
    assert_short_help_shape(&out, "vault repair links");
}

#[test]
fn links_list_short_help() {
    let out = vault_help(&["links", "list", "-h"]);
    assert_short_help_shape(&out, "vault links list");
}

#[test]
fn links_unresolved_short_help() {
    let out = vault_help(&["links", "unresolved", "-h"]);
    assert_short_help_shape(&out, "vault links unresolved");
}

#[test]
fn links_backlinks_short_help() {
    let out = vault_help(&["links", "backlinks", "-h"]);
    assert_short_help_shape(&out, "vault links backlinks");
}

#[test]
fn config_show_short_help() {
    let out = vault_help(&["config", "show", "-h"]);
    assert_short_help_shape(&out, "vault config show");
}

#[test]
fn config_validate_long_help() {
    let out = vault_help(&["config", "validate", "--help"]);
    assert_long_help_shape(&out, "vault config validate");
}

#[test]
fn config_migrate_short_help() {
    let out = vault_help(&["config", "migrate", "-h"]);
    assert_short_help_shape(&out, "vault config migrate");
}

#[test]
fn config_edit_short_help() {
    let out = vault_help(&["config", "edit", "-h"]);
    assert_short_help_shape(&out, "vault config edit");
}

#[test]
fn cache_index_short_help() {
    let out = vault_help(&["cache", "index", "-h"]);
    assert_short_help_shape(&out, "vault cache index");
}

#[test]
fn cache_rebuild_short_help() {
    let out = vault_help(&["cache", "rebuild", "-h"]);
    assert_short_help_shape(&out, "vault cache rebuild");
}

#[test]
fn cache_clear_short_help() {
    let out = vault_help(&["cache", "clear", "-h"]);
    assert_short_help_shape(&out, "vault cache clear");
}

#[test]
fn cache_status_short_help() {
    let out = vault_help(&["cache", "status", "-h"]);
    assert_short_help_shape(&out, "vault cache status");
}

#[test]
fn docs_summary_short_help() {
    let out = vault_help(&["docs", "summary", "-h"]);
    assert_short_help_shape(&out, "vault docs summary");
}

#[test]
fn docs_inspect_short_help() {
    let out = vault_help(&["docs", "inspect", "-h"]);
    assert_short_help_shape(&out, "vault docs inspect");
}

#[test]
fn files_short_help() {
    let out = vault_help(&["files", "-h"]);
    assert_short_help_shape(&out, "vault files");
}

#[test]
fn completions_init_short_help() {
    let out = vault_help(&["completions", "init", "-h"]);
    assert_short_help_shape(&out, "vault completions init");
}

#[test]
fn completions_install_short_help() {
    let out = vault_help(&["completions", "install", "-h"]);
    assert_short_help_shape(&out, "vault completions install");
}

#[test]
fn no_color_strips_ansi() {
    let out = vault_help(&["find", "-h"]);
    // NO_COLOR=1 is already set by the helper. Output must contain no ANSI
    // escape sequences.
    assert!(
        !out.contains('\x1b'),
        "NO_COLOR output should not contain ESC bytes:\n{out:?}"
    );
}

#[test]
fn ascii_fallback_via_env() {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vault"));
    command.args(["find", "-h"]);
    command.env("NO_COLOR", "1");
    command.env("NORN_ASCII", "1");
    let output = command.output().expect("vault command should run");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    // Phase 1 ships no help-specific glyphs (live examples are Phase 3), so
    // the test asserts the binary runs cleanly with NORN_ASCII set —
    // confirming the env doesn't cause a panic or structural change.
    assert!(stdout.contains("USAGE\n"));
}

#[test]
fn root_long_help_has_examples_section() {
    let out = vault_help(&["--help"]);
    assert!(
        out.contains("EXAMPLES\n"),
        "vault --help should include EXAMPLES; got:\n{out}"
    );
    assert!(out.contains("vault find"));
}

#[test]
fn find_long_help_has_examples_with_eq() {
    let out = vault_help(&["find", "--help"]);
    assert!(out.contains("EXAMPLES\n"));
    assert!(
        out.contains("--eq"),
        "find --help EXAMPLES should reference --eq; got:\n{out}"
    );
}

#[test]
fn validate_long_help_has_examples() {
    let out = vault_help(&["validate", "--help"]);
    assert!(out.contains("EXAMPLES\n"));
}

#[test]
fn repair_plan_long_help_has_examples() {
    let out = vault_help(&["repair", "plan", "--help"]);
    assert!(out.contains("EXAMPLES\n"));
}

#[test]
fn repair_apply_long_help_has_examples() {
    let out = vault_help(&["repair", "apply", "--help"]);
    assert!(out.contains("EXAMPLES\n"));
}

#[test]
fn links_unresolved_long_help_has_examples() {
    let out = vault_help(&["links", "unresolved", "--help"]);
    assert!(out.contains("EXAMPLES\n"));
}

#[test]
fn root_short_help_has_no_examples_section() {
    // The short form (-h) must never include EXAMPLES per spec §1.
    let out = vault_help(&["-h"]);
    assert!(
        !out.contains("EXAMPLES"),
        "vault -h must not include EXAMPLES; got:\n{out}"
    );
}

#[test]
fn find_short_help_has_no_examples_section() {
    let out = vault_help(&["find", "-h"]);
    assert!(
        !out.contains("EXAMPLES"),
        "vault find -h must not include EXAMPLES; got:\n{out}"
    );
}

#[test]
fn examples_command_lines_start_with_vault() {
    // Style assertion: every authored example command line begins with the
    // literal `vault ` token (no shell prompts, no leading dashes, no $).
    let out = vault_help(&["find", "--help"]);
    let ex_section_start = out
        .find("EXAMPLES\n")
        .expect("vault find --help has EXAMPLES section");
    let ex_section = &out[ex_section_start..];
    for line in ex_section.lines().skip(1) {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue; // Blank line or comment line.
        }
        if line.starts_with("GLOBAL OPTIONS") || line.starts_with("Documentation:") {
            break; // End of EXAMPLES section.
        }
        assert!(
            trimmed.starts_with("vault "),
            "example command lines must start with 'vault '; got: {line:?}"
        );
    }
}
