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
    prewrite_prune_marker(&tmp.path().join(".xdg-cache"));
    c
}

/// Pre-write a FRESH lazy-sweep throttle marker (`<cache_home>/norn/.last-prune`)
/// so norn invocations under this cache home never spawn a detached GC sweep
/// child (NRN-287) that could race this test. Mirrors src/cache/prune.rs
/// `PRUNE_MARKER`.
fn prewrite_prune_marker(cache_home: &std::path::Path) {
    let tree = cache_home.join("norn");
    let _ = std::fs::create_dir_all(&tree);
    let _ = std::fs::write(tree.join(".last-prune"), b"");
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

// ── R1a: a reserved value flag followed by a flag never silent-empties ──────
#[test]
fn reserved_value_flag_followed_by_flag_is_not_silent_empty() {
    // `--path` is a value flag; the next token `--all` is flag-shaped, so it is
    // NOT consumed as the path value. clap then errors (exit 2, "a value is
    // required") instead of filtering path="--all" and returning an empty set.
    let tmp = synth_vault();
    let out = run(&tmp, &["find", "--path", "--all"]);
    assert!(
        !out.status.success(),
        "must error, not silent-empty (exit 0)"
    );
    assert!(
        out.stdout.is_empty(),
        "no query output: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("a value is required"), "{err}");
}

// ── R3: a repeated dynamic key with a comma-containing value is refused ─────
#[test]
fn repeated_dynamic_key_with_comma_value_is_refused() {
    // `--tag a,b --tag c` cannot losslessly become any-of `--in tag:a,b,c`
    // (three values, not the intended two) — refuse rather than corrupt.
    let tmp = synth_vault();
    let out = run(&tmp, &["find", "--tag", "a,b", "--tag", "c"]);
    assert!(
        !out.status.success(),
        "ambiguous comma case must be refused"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("comma"), "{err}");
    assert!(out.stdout.is_empty());
}

// ── R2: a field declared ONLY via a match-selector / alias_field is accepted ─
fn vault_with_selector_only_field() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-grammar-sel-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    // A document that carries NEITHER `kind` nor `aliases` in its frontmatter,
    // so those fields are known only through config, never observed.
    std::fs::write(root.join("a.md"), "---\ntype: note\n---\nbody\n").unwrap();
    let norn_dir = root.join(".norn");
    std::fs::create_dir(&norn_dir).unwrap();
    // `kind` appears ONLY in a rule's match-selector; `aliases` ONLY as the
    // configured link alias field. Neither is a managed field otherwise.
    std::fs::write(
        norn_dir.join("config.yaml"),
        "links:\n  alias_field: aliases\nvalidate:\n  rules:\n    - match:\n        frontmatter:\n          kind: reference\n",
    )
    .unwrap();
    tmp
}

#[test]
fn match_selector_only_field_is_accepted() {
    let tmp = vault_with_selector_only_field();
    let out = run(&tmp, &["find", "--kind", "reference", "--format", "paths"]);
    assert!(
        out.status.success(),
        "a match-selector-declared field must desugar, not hard-reject; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn alias_field_is_accepted() {
    let tmp = vault_with_selector_only_field();
    let out = run(&tmp, &["find", "--aliases", "foo", "--format", "paths"]);
    assert!(
        out.status.success(),
        "the configured alias_field must be a known field; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ── Control: an undeclared, unobserved field still hard-rejects ─────────────
#[test]
fn undeclared_field_still_rejects() {
    let tmp = vault_with_selector_only_field();
    let out = run(&tmp, &["find", "--zzqqxx", "v"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("unknown field"), "{err}");
}
