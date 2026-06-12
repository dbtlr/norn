//! Integration tests for the custom help renderer.
//!
//! Tests invoke the real binary so they cover the full path: clap parse →
//! intercept → build_model → render → write. Assertions check structural
//! properties (sections present, headings uppercase, globals shown, footer
//! present) rather than exact bytes, so cosmetic edits don't break them.

use std::process::Command;

/// Per-invocation `XDG_CACHE_HOME`/`XDG_STATE_HOME` isolation so the binary
/// never reads or sweeps the developer's real cache/state trees.
fn isolate_xdg(command: &mut Command) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp xdg dir should be created");
    command.env("XDG_CACHE_HOME", dir.path().join("cache"));
    command.env("XDG_STATE_HOME", dir.path().join("state"));
    dir
}

fn norn_help(args: &[&str]) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.args(args);
    // NO_COLOR strips ANSI so assertions match against plain text.
    command.env("NO_COLOR", "1");
    let _xdg = isolate_xdg(&mut command);
    let output = command.output().expect("norn command should run");
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
    let out = norn_help(&["-h"]);
    assert_short_help_shape(&out, "norn");
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
    let out = norn_help(&["--help"]);
    assert_long_help_shape(&out, "norn");
}

#[test]
fn find_short_help() {
    let out = norn_help(&["find", "-h"]);
    assert_short_help_shape(&out, "norn find");
    assert!(out.contains("FILTER")); // some filtering-flavored group exists
}

#[test]
fn find_long_help() {
    let out = norn_help(&["find", "--help"]);
    assert_long_help_shape(&out, "norn find");
}

#[test]
fn init_short_help() {
    let out = norn_help(&["init", "-h"]);
    assert_short_help_shape(&out, "norn init");
}

#[test]
fn init_long_help() {
    let out = norn_help(&["init", "--help"]);
    assert_long_help_shape(&out, "norn init");
}

#[test]
fn validate_short_help() {
    let out = norn_help(&["validate", "-h"]);
    assert_short_help_shape(&out, "norn validate");
}

#[test]
fn validate_long_help() {
    let out = norn_help(&["validate", "--help"]);
    assert_long_help_shape(&out, "norn validate");
}

#[test]
fn repair_short_help() {
    let out = norn_help(&["repair", "-h"]);
    assert_short_help_shape(&out, "norn repair");
}

#[test]
fn repair_long_help() {
    let out = norn_help(&["repair", "--help"]);
    assert_long_help_shape(&out, "norn repair");
}

// repair_links_short_help: removed — vault repair links retired.

#[test]
fn config_show_short_help() {
    let out = norn_help(&["config", "show", "-h"]);
    assert_short_help_shape(&out, "norn config show");
}

#[test]
fn config_validate_long_help() {
    let out = norn_help(&["config", "validate", "--help"]);
    assert_long_help_shape(&out, "norn config validate");
}

#[test]
fn config_migrate_short_help() {
    let out = norn_help(&["config", "migrate", "-h"]);
    assert_short_help_shape(&out, "norn config migrate");
}

#[test]
fn config_edit_short_help() {
    let out = norn_help(&["config", "edit", "-h"]);
    assert_short_help_shape(&out, "norn config edit");
}

#[test]
fn cache_index_short_help() {
    let out = norn_help(&["cache", "index", "-h"]);
    assert_short_help_shape(&out, "norn cache index");
}

#[test]
fn cache_rebuild_short_help() {
    let out = norn_help(&["cache", "rebuild", "-h"]);
    assert_short_help_shape(&out, "norn cache rebuild");
}

#[test]
fn cache_clear_short_help() {
    let out = norn_help(&["cache", "clear", "-h"]);
    assert_short_help_shape(&out, "norn cache clear");
}

#[test]
fn cache_status_short_help() {
    let out = norn_help(&["cache", "status", "-h"]);
    assert_short_help_shape(&out, "norn cache status");
}

#[test]
fn completions_init_short_help() {
    let out = norn_help(&["completions", "init", "-h"]);
    assert_short_help_shape(&out, "norn completions init");
}

#[test]
fn completions_install_short_help() {
    let out = norn_help(&["completions", "install", "-h"]);
    assert_short_help_shape(&out, "norn completions install");
}

#[test]
fn no_color_strips_ansi() {
    let out = norn_help(&["find", "-h"]);
    // NO_COLOR=1 is already set by the helper. Output must contain no ANSI
    // escape sequences.
    assert!(
        !out.contains('\x1b'),
        "NO_COLOR output should not contain ESC bytes:\n{out:?}"
    );
}

#[test]
fn ascii_fallback_via_env() {
    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.args(["find", "-h"]);
    command.env("NO_COLOR", "1");
    command.env("NORN_ASCII", "1");
    let _xdg = isolate_xdg(&mut command);
    let output = command.output().expect("norn command should run");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    // Phase 1 ships no help-specific glyphs (live examples are Phase 3), so
    // the test asserts the binary runs cleanly with NORN_ASCII set —
    // confirming the env doesn't cause a panic or structural change.
    assert!(stdout.contains("USAGE\n"));
}

#[test]
fn root_long_help_has_examples_section() {
    let out = norn_help(&["--help"]);
    assert!(
        out.contains("EXAMPLES\n"),
        "vault --help should include EXAMPLES; got:\n{out}"
    );
    assert!(out.contains("norn find"));
}

#[test]
fn find_long_help_has_examples_with_eq() {
    let out = norn_help(&["find", "--help"]);
    assert!(out.contains("EXAMPLES\n"));
    assert!(
        out.contains("--eq"),
        "find --help EXAMPLES should reference --eq; got:\n{out}"
    );
}

#[test]
fn validate_long_help_has_examples() {
    let out = norn_help(&["validate", "--help"]);
    assert!(out.contains("EXAMPLES\n"));
}

#[test]
fn validate_help_renders_finding_codes_section_with_all_codes() {
    let stdout = norn_help(&["validate", "--help"]);
    assert!(
        stdout.contains("FINDING CODES"),
        "expected FINDING CODES header in --help output; got:\n{stdout}"
    );
    for code in [
        "link-target-missing",
        "link-anchor-missing",
        "link-block-missing",
        "link-ambiguous",
        "frontmatter-required-field-missing",
        "frontmatter-disallowed-value",
        "frontmatter-invalid-type",
        "frontmatter-forbidden-field",
        "frontmatter-alias-shadowed-by-stem",
        "frontmatter-alias-duplicate-across-docs",
        "frontmatter-alias-malformed",
        "document-misrouted",
    ] {
        assert!(
            stdout.contains(code),
            "expected code `{code}` in --help output"
        );
    }
}

#[test]
fn repair_long_help_has_examples() {
    let out = norn_help(&["repair", "--help"]);
    assert!(out.contains("EXAMPLES\n"));
}

#[test]
fn root_short_help_has_no_examples_section() {
    // The short form (-h) must never include EXAMPLES per spec §1.
    let out = norn_help(&["-h"]);
    assert!(
        !out.contains("EXAMPLES"),
        "vault -h must not include EXAMPLES; got:\n{out}"
    );
}

#[test]
fn find_short_help_has_no_examples_section() {
    let out = norn_help(&["find", "-h"]);
    assert!(
        !out.contains("EXAMPLES"),
        "norn find -h must not include EXAMPLES; got:\n{out}"
    );
}

#[test]
fn examples_command_lines_start_with_norn() {
    // Style assertion: every authored example command line begins with the
    // literal `norn ` token (no shell prompts, no leading dashes, no $).
    let out = norn_help(&["find", "--help"]);
    let ex_section_start = out
        .find("EXAMPLES\n")
        .expect("norn find --help has EXAMPLES section");
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
            trimmed.starts_with("norn "),
            "example command lines must start with 'norn '; got: {line:?}"
        );
    }
}

#[test]
fn count_short_help() {
    let out = norn_help(&["count", "-h"]);
    assert!(out.contains("Count documents in the vault"));
}

#[test]
fn get_short_help() {
    let out = norn_help(&["get", "-h"]);
    assert!(out.contains("Get one or more documents"));
}
