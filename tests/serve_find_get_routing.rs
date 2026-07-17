//! End-to-end proof that `norn find` / `norn get` route through the warm daemon
//! byte-identically (NRN-222), including the CLI-side gates and exit signals
//! that the wire does not carry natively.
//!
//! Mirrors `serve_count_routing.rs`: direct output is captured against a private
//! cache home with no daemon socket; routed output against the running daemon's
//! cache home. Routed and direct must match on the FULL (stdout, stderr, exit)
//! triple.

#![cfg(unix)]

#[path = "serve_util/mod.rs"]
mod serve_util;

use serve_util::{
    count_served, norn_bin, read_to_string, socket_path_for, wait_for_ready, ChildGuard,
};

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use tempfile::TempDir;

/// Seed a vault: 2 `type: note`, 1 `type: task`.
fn seed_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-fg-route-vault-")
        .tempdir()
        .expect("tempdir");
    std::fs::write(
        tmp.path().join("note1.md"),
        "---\ntype: note\nstatus: active\ntitle: Note One\n---\nbody one\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("note2.md"),
        "---\ntype: note\nstatus: backlog\ntitle: Note Two\n---\nbody two\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("task1.md"),
        "---\ntype: task\nstatus: backlog\ntitle: Task One\n---\nbody task\n",
    )
    .unwrap();
    tmp
}

/// Pre-write a FRESH lazy-sweep throttle marker (`<cache_home>/norn/.last-prune`)
/// so norn invocations under this cache home never spawn a detached GC sweep
/// child (NRN-287) that could race this test. Mirrors src/cache/prune.rs
/// `PRUNE_MARKER`.
fn prewrite_prune_marker(cache_home: &Path) {
    let tree = cache_home.join("norn");
    let _ = std::fs::create_dir_all(&tree);
    let _ = std::fs::write(tree.join(".last-prune"), b"");
}

/// Run `norn --cwd <vault> <args…>` with the given cache/state homes.
/// Returns `(stdout, stderr, exit_code)`.
fn run_norn(
    cache_home: &Path,
    state_home: &Path,
    vault: &Path,
    args: &[&str],
) -> (Vec<u8>, Vec<u8>, i32) {
    prewrite_prune_marker(cache_home);
    let out = Command::new(norn_bin())
        .env("XDG_CACHE_HOME", cache_home)
        .env("XDG_STATE_HOME", state_home)
        // Generous handshake budget so a daemon scheduled late under CI load
        // still answers the probe (see serve_count_routing.rs).
        .env("NORN_SERVICE_HANDSHAKE_TIMEOUT_MS", "5000")
        .arg("--cwd")
        .arg(vault)
        .args(args)
        .output()
        .expect("run norn");
    (out.stdout, out.stderr, out.status.code().unwrap_or(-1))
}

/// Spawn a daemon on a private cache home, capturing its stderr to a file.
/// Returns (guard, cache_home, state_home, stderr_path, root_tmp).
fn spawn_daemon_logged() -> (
    ChildGuard,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
    TempDir,
) {
    // Short prefix/subdirs: the socket must fit macOS's ~104-byte sun_path.
    let daemon_root = tempfile::Builder::new().prefix("nf-").tempdir().unwrap();
    let cache_home = daemon_root.path().join("c");
    let state_home = daemon_root.path().join("s");
    let stderr_path = daemon_root.path().join("err");
    let stderr_file = std::fs::File::create(&stderr_path).unwrap();
    prewrite_prune_marker(&cache_home);
    let child = Command::new(norn_bin())
        .arg("serve")
        .env("XDG_CACHE_HOME", &cache_home)
        .env("XDG_STATE_HOME", &state_home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .expect("spawn norn serve");
    let guard = ChildGuard(child);
    let socket = socket_path_for(&cache_home);
    wait_for_ready(&socket, Duration::from_secs(10));
    (guard, cache_home, state_home, stderr_path, daemon_root)
}

/// Markdown is a source representation, not a cache facet: a live daemon must
/// serve the request from the vault file and return the exact bytes for the CLI
/// to write, including trailing spaces and an absent final newline.
#[test]
fn routed_get_markdown_is_byte_faithful_and_served() {
    let vault = seed_vault();
    let markdown = b"---\ntype: note\n# formatting only source can preserve\ntitle: 'Exact'\n---\n\n# Heading  \nbody without final newline";
    std::fs::write(vault.path().join("exact.md"), markdown).unwrap();

    let direct_cache = TempDir::new().unwrap();
    let direct_state = TempDir::new().unwrap();
    let args = &["get", "exact.md", "--format", "markdown"][..];
    let direct = run_norn(direct_cache.path(), direct_state.path(), vault.path(), args);
    assert_eq!(direct.2, 0, "direct markdown get exits 0");
    assert_eq!(
        direct.0, markdown,
        "direct output preserves exact source bytes"
    );

    let (_guard, cache_home, state_home, stderr_path, _root) = spawn_daemon_logged();
    let routed = run_norn(&cache_home, &state_home, vault.path(), args);
    assert_eq!(
        routed, direct,
        "routed markdown get must match direct on (stdout, stderr, code)"
    );
    assert_eq!(
        count_served(&stderr_path, "vault.get"),
        1,
        "the daemon must serve markdown rather than falling back to disk locally; stderr:\n{}",
        read_to_string(&stderr_path)
    );

    let multi_args = &["get", "exact.md", "note1.md", "--format", "markdown"][..];
    let direct_multi = run_norn(
        direct_cache.path(),
        direct_state.path(),
        vault.path(),
        multi_args,
    );
    let routed_multi = run_norn(&cache_home, &state_home, vault.path(), multi_args);
    assert_eq!(
        routed_multi, direct_multi,
        "routed multi-document markdown refusal must match direct"
    );
    assert_eq!(
        count_served(&stderr_path, "vault.get"),
        2,
        "the daemon must serve the multi-document refusal too; stderr:\n{}",
        read_to_string(&stderr_path)
    );
}

/// A partially-resolved Markdown request still carries its good document, but
/// the unresolved sibling is an error on both direct and routed paths.
#[test]
fn routed_get_markdown_partial_resolution_matches_direct_error_triple() {
    let vault = seed_vault();
    let args = &["get", "note1.md", "missing.md", "--format", "markdown"][..];

    let direct_cache = TempDir::new().unwrap();
    let direct_state = TempDir::new().unwrap();
    let direct = run_norn(direct_cache.path(), direct_state.path(), vault.path(), args);
    assert_eq!(direct.2, 1, "partial Markdown resolution must exit 1");
    assert!(
        !direct.0.is_empty(),
        "the one resolved document is still emitted"
    );
    assert!(
        String::from_utf8_lossy(&direct.1).contains("did not resolve to any doc"),
        "the unresolved sibling must remain visible: {}",
        String::from_utf8_lossy(&direct.1)
    );

    let (_guard, cache_home, state_home, stderr_path, _root) = spawn_daemon_logged();
    let routed = run_norn(&cache_home, &state_home, vault.path(), args);
    assert_eq!(
        routed, direct,
        "routed partial Markdown must match direct on stdout/stderr/exit"
    );
    assert_eq!(
        count_served(&stderr_path, "vault.get"),
        1,
        "partial Markdown must be served, not fall back; stderr:\n{}",
        read_to_string(&stderr_path)
    );
}

/// `--section` is inert for Markdown, so it must not block routing; both paths
/// warn and still emit the exact whole source document.
#[test]
fn routed_get_markdown_with_section_is_served_and_matches_direct() {
    let vault = seed_vault();
    let args = &[
        "get",
        "note1.md",
        "--section",
        "Does Not Matter",
        "--format",
        "markdown",
    ][..];

    let direct_cache = TempDir::new().unwrap();
    let direct_state = TempDir::new().unwrap();
    let direct = run_norn(direct_cache.path(), direct_state.path(), vault.path(), args);
    assert_eq!(direct.2, 0, "ignored section does not fail Markdown");
    assert!(
        String::from_utf8_lossy(&direct.1).contains("--section is ignored with --format markdown"),
        "direct path must preserve the ignored-section warning: {}",
        String::from_utf8_lossy(&direct.1)
    );

    let (_guard, cache_home, state_home, stderr_path, _root) = spawn_daemon_logged();
    let routed = run_norn(&cache_home, &state_home, vault.path(), args);
    assert_eq!(
        routed, direct,
        "routed Markdown+section must match direct on stdout/stderr/exit"
    );
    assert_eq!(
        count_served(&stderr_path, "vault.get"),
        1,
        "Markdown+section must be served because section is inert; stderr:\n{}",
        read_to_string(&stderr_path)
    );
}

/// A `get` whose target does not resolve must be SERVED by the daemon — exit 1
/// derived from the wire's `error:` note — not bounced to a second, direct
/// execution (NRN-222 review F3). `vault.get` maps the not-found signal to
/// `isError: true`; the routed client must accept that result (it carries the
/// full structuredContent) and reproduce the CLI contract from it, instead of
/// treating it as a transport failure and re-running the whole query directly.
#[test]
fn routed_get_not_found_exits_1_without_direct_fallback() {
    let vault = seed_vault();

    // Direct baseline (no daemon socket in this cache home).
    let direct_cache = TempDir::new().unwrap();
    let direct_state = TempDir::new().unwrap();
    let args = &["get", "no-such-doc"][..];
    let (d_stdout, d_stderr, d_code) =
        run_norn(direct_cache.path(), direct_state.path(), vault.path(), args);
    assert_eq!(d_code, 1, "direct get of a missing target exits 1");
    assert!(
        String::from_utf8_lossy(&d_stderr).contains("error:"),
        "direct get carries the error: note on stderr"
    );

    // Routed: byte-identical triple against a live daemon.
    let (_guard, cache_home, state_home, stderr_path, _root) = spawn_daemon_logged();
    let (r_stdout, r_stderr, r_code) = run_norn(&cache_home, &state_home, vault.path(), args);
    assert_eq!(r_code, d_code, "routed get must exit 1 like direct");
    assert_eq!(r_stdout, d_stdout, "routed get stdout must match direct");
    assert_eq!(r_stderr, d_stderr, "routed get stderr must match direct");

    // Positive routing proof (not vacuous): the daemon emits one per-call
    // "served vault.get" marker for each tools/call it actually serves. A
    // probe that silently decided Direct would leave the counter at zero —
    // and the byte-identity assertions above would still pass — so the marker
    // is what proves the isError result was SERVED, not bounced.
    let served = count_served(&stderr_path, "vault.get");
    assert_eq!(
        served,
        1,
        "the daemon must have served exactly one vault.get for the routed \
         not-found run, got {served}; daemon stderr:\n{}",
        read_to_string(&stderr_path)
    );

    // And with --verbose, a fall-back to Direct would print a "using direct
    // execution" diagnostic — its absence plus the incremented served counter
    // proves the failing get executed exactly once, daemon-side.
    let verbose_args = &["--verbose", "get", "no-such-doc"][..];
    let (_v_stdout, v_stderr, v_code) =
        run_norn(&cache_home, &state_home, vault.path(), verbose_args);
    let v_stderr_text = String::from_utf8_lossy(&v_stderr);
    assert_eq!(v_code, 1, "verbose routed get still exits 1");
    assert!(
        !v_stderr_text.contains("using direct execution"),
        "a not-found get must be served by the daemon, not re-executed \
         directly; verbose stderr shows a fallback:\n{v_stderr_text}"
    );
    let served = count_served(&stderr_path, "vault.get");
    assert_eq!(
        served,
        2,
        "the verbose routed run must also have been served by the daemon \
         (expected 2 total vault.get markers, got {served}); daemon stderr:\n{}",
        read_to_string(&stderr_path)
    );
}

/// NRN-218: a dynamic-field predicate (`find --type note`) must ROUTE warm now,
/// byte-identically to direct, instead of forcing Direct — and an UNKNOWN dynamic
/// field (`find --nonexistentfield x`) must be REFUSED daemon-side with the exact
/// stderr + exit code the direct field-universe gate produces, served (not
/// bounced to a second direct execution).
#[test]
fn routed_dynamic_field_predicate_matches_direct() {
    let vault = seed_vault();

    // Direct baselines (no daemon socket in this cache home).
    let direct_cache = TempDir::new().unwrap();
    let direct_state = TempDir::new().unwrap();

    // A KNOWN dynamic field: `--type note` desugars to `--eq type:note` and,
    // once the daemon gate passes, must return the two notes just like direct.
    let known = &["find", "--type", "note", "--format", "json"][..];
    let d_known = run_norn(
        direct_cache.path(),
        direct_state.path(),
        vault.path(),
        known,
    );
    assert_eq!(d_known.2, 0, "direct known dynamic field exits 0");

    // An UNKNOWN dynamic field: the field-universe gate refuses it (exit 1) with
    // a "unknown field" message on stderr, on both paths.
    let unknown = &["find", "--nonexistentfield", "x"][..];
    let d_unknown = run_norn(
        direct_cache.path(),
        direct_state.path(),
        vault.path(),
        unknown,
    );
    assert_eq!(d_unknown.2, 1, "direct unknown dynamic field exits 1");
    assert!(
        String::from_utf8_lossy(&d_unknown.1).contains("unknown field `nonexistentfield`"),
        "direct unknown field carries the gate message on stderr, got: {:?}",
        String::from_utf8_lossy(&d_unknown.1)
    );

    // Routed: byte-identical triples against a live daemon.
    let (_guard, cache_home, state_home, stderr_path, _root) = spawn_daemon_logged();

    let r_known = run_norn(&cache_home, &state_home, vault.path(), known);
    assert_eq!(
        r_known, d_known,
        "routed KNOWN dynamic field must match direct on (stdout, stderr, code)"
    );

    let r_unknown = run_norn(&cache_home, &state_home, vault.path(), unknown);
    assert_eq!(
        r_unknown.2, d_unknown.2,
        "routed UNKNOWN dynamic field must exit 1 like direct"
    );
    assert_eq!(
        r_unknown.0, d_unknown.0,
        "routed UNKNOWN dynamic field stdout must match direct"
    );
    assert_eq!(
        r_unknown.1, d_unknown.1,
        "routed UNKNOWN dynamic field stderr must be byte-identical to direct\nrouted: {:?}\ndirect: {:?}",
        String::from_utf8_lossy(&r_unknown.1),
        String::from_utf8_lossy(&d_unknown.1),
    );

    // Both dynamic-predicate runs must have been SERVED by the daemon (the whole
    // point of NRN-218), including the refusal — a served marker fires once per
    // tools/call the daemon handles, so a refusal that fell back to Direct would
    // leave this at 1, not 2.
    let served = count_served(&stderr_path, "vault.find");
    assert_eq!(
        served,
        2,
        "the daemon must have served BOTH dynamic-field finds (known + unknown), got {served}; \
         daemon stderr:\n{}",
        read_to_string(&stderr_path)
    );

    // And with --verbose, the refused dynamic field must not fall back to Direct:
    // a fallback would print "using direct execution" and re-execute the gate.
    let verbose_unknown = &["--verbose", "find", "--nonexistentfield", "x"][..];
    let (_v_stdout, v_stderr, v_code) =
        run_norn(&cache_home, &state_home, vault.path(), verbose_unknown);
    assert_eq!(
        v_code, 1,
        "verbose routed unknown dynamic field still exits 1"
    );
    assert!(
        !String::from_utf8_lossy(&v_stderr).contains("using direct execution"),
        "a refused dynamic field must be served daemon-side, not re-executed directly; \
         verbose stderr:\n{}",
        String::from_utf8_lossy(&v_stderr)
    );
    let served = count_served(&stderr_path, "vault.find");
    assert_eq!(
        served,
        3,
        "the verbose refused run must also have been served (expected 3 total), got {served}; \
         daemon stderr:\n{}",
        read_to_string(&stderr_path)
    );
}

/// The missing-predicate help gate must hold on the routed path exactly as on
/// the direct path: a bare `norn find` (no predicates, no --all) — and its
/// `--text ""` twin, which `has_predicate` treats as no predicate — prints the
/// find help to stderr and exits 2 instead of dumping the vault through the
/// daemon (NRN-222 review F1).
#[test]
fn routed_find_respects_missing_predicate_gate() {
    let vault = seed_vault();

    // Direct baselines (no daemon socket in this cache home).
    let direct_cache = TempDir::new().unwrap();
    let direct_state = TempDir::new().unwrap();
    let gate_shapes: Vec<Vec<&str>> = vec![vec!["find"], vec!["find", "--text", ""]];
    let direct: Vec<_> = gate_shapes
        .iter()
        .map(|shape| {
            run_norn(
                direct_cache.path(),
                direct_state.path(),
                vault.path(),
                shape,
            )
        })
        .collect();
    for (shape, (stdout, stderr, code)) in gate_shapes.iter().zip(direct.iter()) {
        assert_eq!(*code, 2, "direct bare {shape:?} must exit 2 (help gate)");
        assert!(
            stdout.is_empty(),
            "direct bare {shape:?} must print nothing to stdout"
        );
        assert!(
            !stderr.is_empty(),
            "direct bare {shape:?} must print help to stderr"
        );
    }

    // Routed: same commands against a live daemon must behave identically.
    let (_guard, cache_home, state_home, stderr_path, _root) = spawn_daemon_logged();

    // Positive control FIRST: a predicated find against the same daemon must
    // actually route (one served marker) and match its direct baseline — this
    // is what keeps the zero-marker assertion below from passing vacuously
    // (e.g. if the probe were broken and everything silently ran Direct).
    let control = &["find", "--eq", "type:note"][..];
    let d_control = run_norn(
        direct_cache.path(),
        direct_state.path(),
        vault.path(),
        control,
    );
    let r_control = run_norn(&cache_home, &state_home, vault.path(), control);
    assert_eq!(
        r_control, d_control,
        "control predicated find must match direct on (stdout, stderr, code)"
    );
    let served = count_served(&stderr_path, "vault.find");
    assert_eq!(
        served,
        1,
        "the control predicated find must have been served by the daemon \
         (expected 1 vault.find marker, got {served}); daemon stderr:\n{}",
        read_to_string(&stderr_path)
    );

    for (shape, (direct_stdout, direct_stderr, direct_code)) in
        gate_shapes.iter().zip(direct.iter())
    {
        let (stdout, stderr, code) = run_norn(&cache_home, &state_home, vault.path(), shape);
        assert_eq!(
            code, *direct_code,
            "routed bare {shape:?} must exit 2 like direct (help gate), got {code}"
        );
        assert_eq!(
            stdout,
            *direct_stdout,
            "routed bare {shape:?} stdout must match direct\nrouted: {:?}",
            String::from_utf8_lossy(&stdout)
        );
        assert_eq!(
            stderr, *direct_stderr,
            "routed bare {shape:?} stderr must match direct"
        );
    }

    // The gated shapes must never have reached the daemon: the served counter
    // stays at the control's 1.
    let served = count_served(&stderr_path, "vault.find");
    assert_eq!(
        served,
        1,
        "gated shapes must NOT route: expected the vault.find served counter \
         to stay at 1 (the control), got {served}; daemon stderr:\n{}",
        read_to_string(&stderr_path)
    );
}
