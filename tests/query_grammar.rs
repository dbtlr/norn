//! End-to-end tests for the ADR 0010 forgiving query grammar (NRN-206/207/209):
//! separator forgiveness, dynamic field predicates + the field-universe gate,
//! and the hidden alias pack — exercised through the real binary so the
//! pre-clap normalization pass and the post-cache gate are both covered.

use std::process::Command;
use tempfile::TempDir;

fn synth_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-grammar-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("a.md"),
        "---\ntype: note\nstatus: active\nmodified: 2026-07-01\n---\nbody\n",
    )
    .unwrap();
    std::fs::write(
        root.join("b.md"),
        "---\ntype: note\nstatus: backlog\nmodified: 2026-06-01\n---\nbody\n",
    )
    .unwrap();
    std::fs::write(
        root.join("c.md"),
        "---\ntype: log\nstatus: backlog\nmodified: 2026-05-01\n---\nbody\n",
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

fn norn_cmd(tmp: &TempDir) -> Command {
    let mut c = Command::new(norn_bin());
    c.env("XDG_CACHE_HOME", tmp.path().join(".xdg-cache"))
        .env("XDG_STATE_HOME", tmp.path().join(".xdg-state"));
    c
}

fn run(tmp: &TempDir, args: &[&str]) -> std::process::Output {
    let mut c = norn_cmd(tmp);
    c.args(["--cwd"]).arg(tmp.path().join("vault")).args(args);
    c.output().unwrap()
}

// ── T2: dynamic field predicate == canonical --eq ──────────────────────────
#[test]
fn dynamic_predicate_matches_canonical_eq() {
    let tmp = synth_vault();
    let dynamic = run(&tmp, &["find", "--type", "note", "--format", "paths"]);
    let canonical = run(&tmp, &["find", "--eq", "type:note", "--format", "paths"]);
    assert!(
        dynamic.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&dynamic.stderr)
    );
    assert!(canonical.status.success());
    assert_eq!(dynamic.stdout, canonical.stdout);
    // Two note docs.
    let s = String::from_utf8_lossy(&dynamic.stdout);
    assert_eq!(s.lines().filter(|l| !l.is_empty()).count(), 2, "{s}");
}

// ── T1: `=` separator on a predicate value ─────────────────────────────────
#[test]
fn equals_separator_on_predicate_parses() {
    let tmp = synth_vault();
    let out = run(
        &tmp,
        &["find", "--eq", "modified=2026-07-01", "--format", "paths"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("a.md"), "{s}");
}

// ── T2b: typo'd real flag HARD-ERRORS with did-you-mean (no silent empty) ──
#[test]
fn typo_of_real_flag_hard_errors_with_suggestion() {
    let tmp = synth_vault();
    let out = run(&tmp, &["find", "--formt", "json", "--type", "note"]);
    assert!(
        !out.status.success(),
        "a typo'd flag must not succeed silently"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("--format"),
        "expected did-you-mean --format, got: {err}"
    );
    // And it must NOT have produced query output.
    assert!(
        out.stdout.is_empty(),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

// ── T2b: unknown non-field key hard-errors ─────────────────────────────────
#[test]
fn unknown_non_field_key_errors() {
    let tmp = synth_vault();
    let out = run(&tmp, &["find", "--zzqqxx", "value"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("unknown field"), "{err}");
}

// ── T2: bare unknown flag with no value errors ─────────────────────────────
#[test]
fn bare_unknown_flag_errors() {
    let tmp = synth_vault();
    let out = run(&tmp, &["find", "--draft"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("requires a value"), "{err}");
}

// ── T2: repeated dynamic key desugars to any-of ────────────────────────────
#[test]
fn repeated_dynamic_key_is_any_of() {
    let tmp = synth_vault();
    let dynamic = run(
        &tmp,
        &[
            "find", "--status", "active", "--status", "backlog", "--format", "paths",
        ],
    );
    let canonical = run(
        &tmp,
        &["find", "--in", "status:active,backlog", "--format", "paths"],
    );
    assert!(
        dynamic.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&dynamic.stderr)
    );
    assert_eq!(dynamic.stdout, canonical.stdout);
    // All three docs (active + two backlog).
    let s = String::from_utf8_lossy(&dynamic.stdout);
    assert_eq!(s.lines().filter(|l| !l.is_empty()).count(), 3, "{s}");
}

// ── T3: --group-by aliases --by on count ───────────────────────────────────
#[test]
fn group_by_alias_matches_by_on_count() {
    let tmp = synth_vault();
    let alias = run(&tmp, &["count", "--group-by", "type", "--format", "json"]);
    let canonical = run(&tmp, &["count", "--by", "type", "--format", "json"]);
    assert!(
        alias.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&alias.stderr)
    );
    assert_eq!(alias.stdout, canonical.stdout);
}

// ── T3: count --all is an accepted no-op ───────────────────────────────────
#[test]
fn count_all_is_accepted_noop() {
    let tmp = synth_vault();
    let with_all = run(&tmp, &["count", "--all", "--format", "json"]);
    let without = run(&tmp, &["count", "--format", "json"]);
    assert!(
        with_all.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&with_all.stderr)
    );
    assert_eq!(with_all.stdout, without.stdout);
}

// ── T3: --where / --filter alias --eq ──────────────────────────────────────
#[test]
fn where_alias_matches_eq() {
    let tmp = synth_vault();
    let alias = run(&tmp, &["find", "--where", "type:note", "--format", "paths"]);
    let canonical = run(&tmp, &["find", "--eq", "type:note", "--format", "paths"]);
    assert!(
        alias.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&alias.stderr)
    );
    assert_eq!(alias.stdout, canonical.stdout);
}

// ── T1: cross-family teaching error on `set --eq` ──────────────────────────
#[test]
fn set_eq_teaches_field() {
    let tmp = synth_vault();
    let out = run(&tmp, &["set", "a.md", "--eq", "status:done"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--field key=value"), "{err}");
}
