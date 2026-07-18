//! End-to-end proof that `norn new` routes through the warm daemon (NRN-230 PR
//! C) byte-identically — the fifth routed mutation, following `set`/`edit` and
//! the `move`/`delete`/`rewrite-wikilink` cascade.
//!
//! The load-bearing invariant mirrors `serve_set_routing`: routed and direct
//! output must be BYTE-IDENTICAL for every mode and format. Because a creation is
//! stateful, each case seeds TWO identical vault copies — one run direct (a
//! private `XDG_CACHE_HOME` with no daemon socket), one run routed (the daemon's
//! cache home) — and asserts:
//!
//! 1. the `(stdout, stderr, exit_code)` triple is byte-identical (modulo the
//!    non-deterministic telemetry `trace_id`, redacted on BOTH sides — two DIRECT
//!    applies also mint different trace ids, so identity there is impossible by
//!    construction, not a routing defect);
//! 2. the POST-state vault is byte-for-byte identical across the two copies
//!    (every file — created doc bytes, dirs, config — not just the target),
//!    which also pins the NRN-114 empty-frontmatter created-file bytes;
//! 3. the daemon actually SERVED the call (its per-call `served vault.new`
//!    marker), so a silent fall-back to Direct — which would make (1) and (2)
//!    pass vacuously — goes red.
//!
//! `new` differs from `set` on two axes exercised here:
//! - **Refusals have no JSON error envelope (audit F5):** a refusal is `error:
//!   {message}` prose on stderr + exit 2 in BOTH formats.
//! - **`--field-json` ROUTES** (unlike `set`, which gates it): `vault.new`'s
//!   params carry it as an ordered `Vec<String>`, so the wire preserves order.
//!
//! One contention case pins the routed apply as SAFE under a held lock (non-zero
//! exit, nothing written) WITHOUT asserting byte-identity: the two legitimately
//! diverge (a routed apply that fails after send surfaces `post-send-uncertain` /
//! exit 1; a direct new that cannot take the lock exits 2 with a hardcoded
//! lock-timeout line whose prose differs from the daemon's coded refusal). Safety
//! over a forced assertion, exactly as `serve_set_routing` does.

#![cfg(unix)]

#[path = "serve_util/mod.rs"]
mod serve_util;

use serve_util::{
    count_served, dir_snapshot, norn_bin, spawn_ready_daemon_with_log, write_vault, DaemonWithLog,
};

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use tempfile::TempDir;

/// One shape row in a byte-identity matrix: `(name, seed, args, expected_exit)`.
type Case<'a> = (&'a str, &'a [(&'a str, &'a str)], Vec<&'a str>, i32);

/// A fresh vault tempdir with a NON-dot prefix. `TempDir::new()` names dirs
/// `.tmpXXXX` (leading dot), and norn's walker skips dot-prefixed path components
/// — so a hidden vault root makes every doc invisible. The read + set suites
/// dodge this the same way.
fn fresh_vault() -> TempDir {
    tempfile::Builder::new()
        .prefix("norn-new-route-vault-")
        .tempdir()
        .expect("vault tempdir")
}

// ── Seed configs ────────────────────────────────────────────────────────────

/// Base config: a rule matching every `.md` (scaffolds `type: note`), a
/// configured `inbox` (Mode C), and a `files.ignore` glob (the path-ignored
/// refusal). Covers Mode A, Mode C, and most refusals.
const CONFIG_BASE: &str = "\
inbox:
  path: Inbox
files:
  ignore:
    - \"scratch/**\"
validate:
  rules:
    - name: any
      match:
        path: \"**/*.md\"
      frontmatter_defaults:
        type: note
";

/// Rule config for Mode B: a creatable `task` rule (target + body scaffold + a
/// `{{var.workspace}}` hole), a non-creatable `no-target-rule`, and a
/// `bad-body` rule whose scaffold references an unknown variable. NO `inbox`, so
/// the no-inbox-configured refusal fires here.
const CONFIG_RULE: &str = "\
validate:
  rules:
    - name: task
      target: \"Workspaces/{{var.workspace}}/tasks/{{title|slugify}}.md\"
      body: \"## Context\\n\"
      frontmatter_defaults:
        type: task
    - name: no-target-rule
      match:
        path: \"docs/**\"
      frontmatter_defaults:
        type: doc
    - name: bad-body
      target: \"badbody.md\"
      body: \"hello {{bogus}}\"
";

/// Empty schema: no rule matches, so a created doc scaffolds NOTHING — the
/// NRN-114 empty-frontmatter case. The post-state snapshot pins the created
/// file's exact bytes.
const CONFIG_EMPTY: &str = "validate: {}\n";

/// A `{{seq}}` rule for the incremental-id tests.
const CONFIG_SEQ: &str = "\
validate:
  rules:
    - name: seqtask
      target: \"seq/MMR-{{seq}}.md\"
      frontmatter_defaults:
        type: task
";

/// Base seed: `CONFIG_BASE` + an `exists.md` (destination-exists) + an untouched
/// `note.md` (proves the WHOLE post-state vault, not just the target, matches).
fn base_seed() -> Vec<(&'static str, &'static str)> {
    vec![
        (".norn/config.yaml", CONFIG_BASE),
        ("exists.md", "---\ntype: note\n---\nalready here\n"),
        (
            "note.md",
            "---\ntype: note\ntitle: A Note\n---\nNote body\n",
        ),
    ]
}

fn rule_seed() -> Vec<(&'static str, &'static str)> {
    vec![(".norn/config.yaml", CONFIG_RULE)]
}

fn empty_seed() -> Vec<(&'static str, &'static str)> {
    vec![(".norn/config.yaml", CONFIG_EMPTY)]
}

fn seq_seed() -> Vec<(&'static str, &'static str)> {
    vec![(".norn/config.yaml", CONFIG_SEQ)]
}

// ── Runners ─────────────────────────────────────────────────────────────────

/// Pre-write a FRESH lazy-sweep throttle marker (`<cache_home>/norn/.last-prune`)
/// so norn invocations under this cache home never spawn a detached GC sweep
/// child (NRN-287) that could race this test. Mirrors src/cache/prune.rs
/// `PRUNE_MARKER`.
fn prewrite_prune_marker(cache_home: &Path) {
    let tree = cache_home.join("norn");
    std::fs::create_dir_all(&tree).expect("NRN-287 sweep isolation: pre-write throttle-marker dir");
    std::fs::write(tree.join(".last-prune"), b"")
        .expect("NRN-287 sweep isolation: pre-write throttle marker");
}

/// Run `norn --cwd <vault> new <args>` with the given cache/state homes. Stdin is
/// forced to `/dev/null` (never a terminal) and stdout is captured (a pipe, never
/// a terminal), so `new`'s "non-TTY without --yes = implicit dry-run preview"
/// path is exercised deterministically regardless of how the test binary was
/// launched.
fn run_new(
    cache_home: &Path,
    state_home: &Path,
    vault: &Path,
    args: &[&str],
) -> (Vec<u8>, Vec<u8>, i32) {
    prewrite_prune_marker(cache_home);
    let out = Command::new(norn_bin())
        .env("XDG_CACHE_HOME", cache_home)
        .env("XDG_STATE_HOME", state_home)
        // A generous handshake budget so a late-scheduled daemon still answers
        // the probe under CI load (the per-shape served-count proof turns a
        // silent fall-back into a hard failure). Harmless on direct runs.
        .env("NORN_SERVICE_HANDSHAKE_TIMEOUT_MS", "5000")
        .stdin(Stdio::null())
        .arg("--cwd")
        .arg(vault)
        .arg("new")
        .args(args)
        .output()
        .expect("run norn new");
    (out.stdout, out.stderr, out.status.code().unwrap_or(-1))
}

/// Like [`run_new`] but feeds `stdin` to the process (for `--body-from-stdin`).
fn run_new_with_stdin(
    cache_home: &Path,
    state_home: &Path,
    vault: &Path,
    args: &[&str],
    stdin: &str,
) -> (Vec<u8>, Vec<u8>, i32) {
    prewrite_prune_marker(cache_home);
    let mut child = Command::new(norn_bin())
        .env("XDG_CACHE_HOME", cache_home)
        .env("XDG_STATE_HOME", state_home)
        .env("NORN_SERVICE_HANDSHAKE_TIMEOUT_MS", "5000")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .arg("--cwd")
        .arg(vault)
        .arg("new")
        .args(args)
        .spawn()
        .expect("spawn norn new");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait norn new");
    (out.stdout, out.stderr, out.status.code().unwrap_or(-1))
}

// ── trace_id redaction ──────────────────────────────────────────────────────

/// Redact the non-deterministic telemetry `trace_id` so a routed apply compares
/// to a direct one. Two carriers: the records `trace: <id>` footer line, and the
/// pretty-JSON `"trace_id": "<id>"` field (2-space indent → a SPACE after the
/// colon, unlike `set`'s compact envelope). Applied to BOTH sides, so a genuine
/// trailing-newline / layout divergence still surfaces. Dry-run/preview/refusal
/// outputs carry an empty trace id, so the transform is a consistent no-op there.
fn redact_trace(bytes: &[u8]) -> String {
    let input = String::from_utf8_lossy(bytes);
    let mut result = String::with_capacity(input.len());
    for segment in input.split_inclusive('\n') {
        let (line, nl) = match segment.strip_suffix('\n') {
            Some(l) => (l, "\n"),
            None => (segment, ""),
        };
        if line.starts_with("trace: ") {
            result.push_str("trace: <redacted>");
        } else {
            result.push_str(&redact_json_trace_id(line));
        }
        result.push_str(nl);
    }
    result
}

fn redact_json_trace_id(line: &str) -> String {
    const KEY: &str = "\"trace_id\": \"";
    if let Some(start) = line.find(KEY) {
        let after = start + KEY.len();
        if let Some(end_rel) = line[after..].find('"') {
            let mut s = String::with_capacity(line.len());
            s.push_str(&line[..after]);
            s.push_str("<redacted>");
            s.push_str(&line[after + end_rel..]);
            return s;
        }
    }
    line.to_string()
}

// ── The byte-identity comparison ────────────────────────────────────────────

/// One routable shape: seed two identical fresh copies, run one direct and one
/// routed, and assert the triple + full post-state vault are byte-identical and
/// that the daemon served exactly one more `vault.new`.
#[allow(clippy::too_many_arguments)]
fn assert_routed_matches_direct(
    daemon: &DaemonWithLog,
    name: &str,
    seed: &[(&str, &str)],
    args: &[&str],
    expected_exit: i32,
    expected_served: &mut usize,
) {
    let direct_vault = fresh_vault();
    let routed_vault = fresh_vault();
    write_vault(direct_vault.path(), seed);
    write_vault(routed_vault.path(), seed);

    // Direct: a private cache/state home with no daemon socket.
    let direct_cache = TempDir::new().unwrap();
    let direct_state = TempDir::new().unwrap();
    let (d_out, d_err, d_code) = run_new(
        direct_cache.path(),
        direct_state.path(),
        direct_vault.path(),
        args,
    );
    assert_eq!(
        d_code,
        expected_exit,
        "[{name}] direct exit code sanity ({expected_exit} expected), got {d_code}; stderr: {}",
        String::from_utf8_lossy(&d_err)
    );

    // Routed: the CLI's cache home IS the daemon's, so its probe routes.
    let (r_out, r_err, r_code) = run_new(
        &daemon.cache_home,
        &daemon.state_home,
        routed_vault.path(),
        args,
    );

    // (1) triple identity (trace ids redacted on both sides).
    assert_eq!(
        redact_trace(&r_out),
        redact_trace(&d_out),
        "[{name}] routed stdout must match direct\nrouted: {:?}\ndirect: {:?}",
        String::from_utf8_lossy(&r_out),
        String::from_utf8_lossy(&d_out),
    );
    assert_eq!(
        r_err,
        d_err,
        "[{name}] routed stderr must match direct\nrouted: {:?}\ndirect: {:?}",
        String::from_utf8_lossy(&r_err),
        String::from_utf8_lossy(&d_err),
    );
    assert_eq!(
        r_code, d_code,
        "[{name}] routed exit code must match direct"
    );

    // (2) whole post-state vault identity — created doc bytes, dirs, and config.
    assert_eq!(
        dir_snapshot(direct_vault.path()),
        dir_snapshot(routed_vault.path()),
        "[{name}] the whole post-state vault must be byte-identical across direct and routed",
    );

    // (3) the daemon served exactly one more vault.new for this shape.
    *expected_served += 1;
    let served = count_served(&daemon.stderr_path, "vault.new");
    assert_eq!(
        served,
        *expected_served,
        "[{name}] the daemon must have SERVED this routed new (running total {expected_served}), \
         got {served}; a silent fall-back to Direct would make identity pass vacuously.\n\
         daemon stderr:\n{}",
        std::fs::read_to_string(&daemon.stderr_path).unwrap_or_default(),
    );
}

/// Every routable shape: routed output is byte-identical to direct (triple + full
/// post-state vault), and each routed shape was SERVED exactly once. Covers all
/// three creation modes × {dry-run, apply} × {records, json}, the title-ignored
/// warning, a valid `--field-json` (routes, unlike `set`), the NRN-114
/// empty-frontmatter created-file bytes, and every coded refusal.
#[test]
fn routed_new_is_byte_identical_to_direct() {
    let daemon = spawn_ready_daemon_with_log(&[]);
    let mut served = 0usize;

    let base = base_seed();
    let rule = rule_seed();
    let empty = empty_seed();

    // (name, seed, args, expected_exit) — see the module-level `Case` alias.
    let cases: Vec<Case> = vec![
        // ── Mode A (explicit path) × {dry-run, apply} × {records, json} ──
        ("mode A records apply", &base, vec!["a.md", "--yes"], 0),
        (
            "mode A json apply",
            &base,
            vec!["a.md", "--yes", "--format", "json"],
            0,
        ),
        (
            "mode A records dry-run",
            &base,
            vec!["a.md", "--dry-run"],
            0,
        ),
        (
            "mode A json dry-run",
            &base,
            vec!["a.md", "--dry-run", "--format", "json"],
            0,
        ),
        (
            "mode A json preview (no --yes)",
            &base,
            vec!["a.md", "--format", "json"],
            0,
        ),
        ("mode A records preview (non-tty)", &base, vec!["a.md"], 0),
        // ── F1: Mode A + --title warns title-ignored, both formats ──
        (
            "mode A + title warns (records)",
            &base,
            vec!["a.md", "--title", "Ignored", "--yes"],
            0,
        ),
        (
            "mode A + title warns (json)",
            &base,
            vec!["a.md", "--title", "Ignored", "--yes", "--format", "json"],
            0,
        ),
        // ── --field-json ROUTES (unlike set): valid typed value, applied ──
        (
            "mode A field-json apply (json)",
            &base,
            vec![
                "a.md",
                "--field-json",
                "tags=[\"x\",\"y\"]",
                "--yes",
                "--format",
                "json",
            ],
            0,
        ),
        // ── Mode C (inbox fallback) × {dry-run, apply} × {records, json} ──
        (
            "mode C inbox records apply",
            &base,
            vec!["--title", "My Note", "--parents", "--yes"],
            0,
        ),
        (
            "mode C inbox json apply",
            &base,
            vec![
                "--title",
                "My Note",
                "--parents",
                "--yes",
                "--format",
                "json",
            ],
            0,
        ),
        (
            "mode C inbox dry-run",
            &base,
            vec!["--title", "My Note", "--parents", "--dry-run"],
            0,
        ),
        // ── Mode B (rule-targeted) × {dry-run, apply} × {records, json} ──
        (
            "mode B records apply",
            &rule,
            vec![
                "--as",
                "task",
                "--title",
                "Fix It",
                "--var",
                "workspace=norn",
                "--parents",
                "--yes",
            ],
            0,
        ),
        (
            "mode B json apply",
            &rule,
            vec![
                "--as",
                "task",
                "--title",
                "Fix It",
                "--var",
                "workspace=norn",
                "--parents",
                "--yes",
                "--format",
                "json",
            ],
            0,
        ),
        (
            "mode B records dry-run",
            &rule,
            vec![
                "--as",
                "task",
                "--title",
                "Fix It",
                "--var",
                "workspace=norn",
                "--parents",
                "--dry-run",
            ],
            0,
        ),
        // ── NRN-114: empty-frontmatter created-file bytes ──
        (
            "empty-frontmatter records apply",
            &empty,
            vec!["bare.md", "--yes"],
            0,
        ),
        (
            "empty-frontmatter json apply",
            &empty,
            vec!["bare.md", "--yes", "--format", "json"],
            0,
        ),
        // ── Coded refusals (exit 2), records + json where the format matters ──
        (
            "refusal destination-exists (records)",
            &base,
            vec!["exists.md", "--yes"],
            2,
        ),
        (
            "refusal destination-exists (json)",
            &base,
            vec!["exists.md", "--yes", "--format", "json"],
            2,
        ),
        (
            "refusal parent-missing",
            &base,
            vec!["deep/nested/x.md", "--yes"],
            2,
        ),
        (
            "refusal containment (.. escape)",
            &base,
            vec!["../escape.md", "--yes"],
            2,
        ),
        (
            "refusal unknown-rule",
            &base,
            vec!["--as", "nope", "--yes"],
            2,
        ),
        (
            "refusal path-and-rule-conflict",
            &base,
            vec!["a.md", "--as", "any", "--yes"],
            2,
        ),
        (
            "refusal invalid --field-json",
            &base,
            vec!["a.md", "--field-json", "tags={bad", "--yes"],
            2,
        ),
        (
            "refusal path-ignored",
            &base,
            vec!["scratch/x.md", "--parents", "--yes"],
            2,
        ),
        (
            "refusal rule-not-creatable",
            &rule,
            vec!["--as", "no-target-rule", "--yes"],
            2,
        ),
        (
            "refusal missing-var",
            &rule,
            vec!["--as", "task", "--title", "Fix It", "--parents", "--yes"],
            2,
        ),
        (
            "refusal no-inbox-configured",
            &rule,
            vec!["--title", "Orphan", "--yes"],
            2,
        ),
        (
            "refusal body-scaffold render failure",
            &rule,
            vec!["--as", "bad-body", "--yes"],
            2,
        ),
    ];

    for (name, seed, args, expected_exit) in cases {
        assert_routed_matches_direct(&daemon, name, seed, &args, expected_exit, &mut served);
    }
}

/// With NO daemon, every routable shape runs the direct path cleanly (the
/// fallback-is-total half of the invariant): the routing seam returns `None` and
/// today's behavior is unchanged. A representative slice (one per mode/refusal
/// family) keeps this fast.
#[test]
fn no_daemon_runs_direct() {
    let base = base_seed();
    let rule = rule_seed();
    let cases: Vec<Case> = vec![
        ("mode A apply", &base, vec!["a.md", "--yes"], 0),
        (
            "mode B apply",
            &rule,
            vec![
                "--as",
                "task",
                "--title",
                "Fix It",
                "--var",
                "workspace=norn",
                "--parents",
                "--yes",
            ],
            0,
        ),
        (
            "mode C inbox apply",
            &base,
            vec!["--title", "My Note", "--parents", "--yes"],
            0,
        ),
        (
            "refusal destination-exists",
            &base,
            vec!["exists.md", "--yes"],
            2,
        ),
    ];
    for (name, seed, args, expected_exit) in cases {
        let vault = fresh_vault();
        write_vault(vault.path(), seed);
        let cache = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let (_out, err, code) = run_new(cache.path(), state.path(), vault.path(), &args);
        assert_eq!(
            code,
            expected_exit,
            "[{name}] new should exit {expected_exit} with no daemon, got {code}; stderr: {}",
            String::from_utf8_lossy(&err)
        );
    }
}

// ── {{seq}} allocation ──────────────────────────────────────────────────────

/// `{{seq}}` allocation is apply-time under the flock from a live directory scan;
/// dry-run prediction is non-binding. This proves:
/// 1. a routed dry-run PREDICTS the next id byte-identically to a direct dry-run
///    (both scan the same empty seq dir → `seq/MMR-1.md`, path stays templated);
/// 2. interleaved routed + direct applies against the SAME vault each RESOLVE a
///    unique concrete id (MMR-1, MMR-2, MMR-3) — the daemon and the CLI both scan
///    the same filesystem under the lock, so neither collides (the #114
///    same-file guarantee: routed and direct resolve to the same target).
#[test]
fn routed_seq_dry_run_predicts_and_interleaved_applies_stay_unique() {
    let daemon = spawn_ready_daemon_with_log(&[]);
    let seed = seq_seed();

    // (1) Prediction byte-identity on two fresh copies.
    let direct_vault = fresh_vault();
    let routed_vault = fresh_vault();
    write_vault(direct_vault.path(), &seed);
    write_vault(routed_vault.path(), &seed);
    let dcache = TempDir::new().unwrap();
    let dstate = TempDir::new().unwrap();
    let dry_args = &[
        "--as",
        "seqtask",
        "--parents",
        "--dry-run",
        "--format",
        "json",
    ];
    let (d_out, _d_err, d_code) =
        run_new(dcache.path(), dstate.path(), direct_vault.path(), dry_args);
    let (r_out, _r_err, r_code) = run_new(
        &daemon.cache_home,
        &daemon.state_home,
        routed_vault.path(),
        dry_args,
    );
    assert_eq!(d_code, 0, "direct seq dry-run must succeed");
    assert_eq!(r_code, 0, "routed seq dry-run must succeed");
    assert_eq!(
        redact_trace(&r_out),
        redact_trace(&d_out),
        "routed seq dry-run prediction must match direct\nrouted: {:?}\ndirect: {:?}",
        String::from_utf8_lossy(&r_out),
        String::from_utf8_lossy(&d_out),
    );
    let d_json: serde_json::Value = serde_json::from_slice(&d_out).unwrap();
    assert_eq!(
        d_json["path"], "seq/MMR-{{seq}}.md",
        "dry-run path stays templated"
    );
    assert_eq!(
        d_json["predicted_path"], "seq/MMR-1.md",
        "dry-run must predict the next id non-bindingly"
    );
    assert!(
        !routed_vault.path().join("seq").exists(),
        "routed dry-run must write nothing"
    );

    // (2) Interleaved routed → direct → routed applies against ONE shared vault.
    let vault = fresh_vault();
    write_vault(vault.path(), &seed);
    let apply_args = &["--as", "seqtask", "--parents", "--yes"];

    // routed apply → MMR-1
    let (_o, e, c) = run_new(
        &daemon.cache_home,
        &daemon.state_home,
        vault.path(),
        apply_args,
    );
    assert_eq!(
        c,
        0,
        "routed apply 1 failed: {}",
        String::from_utf8_lossy(&e)
    );

    // direct apply (fresh cache, no daemon) against the SAME vault → MMR-2
    let idcache = TempDir::new().unwrap();
    let idstate = TempDir::new().unwrap();
    let (_o, e, c) = run_new(idcache.path(), idstate.path(), vault.path(), apply_args);
    assert_eq!(
        c,
        0,
        "direct apply 2 failed: {}",
        String::from_utf8_lossy(&e)
    );

    // routed apply → MMR-3
    let (_o, e, c) = run_new(
        &daemon.cache_home,
        &daemon.state_home,
        vault.path(),
        apply_args,
    );
    assert_eq!(
        c,
        0,
        "routed apply 3 failed: {}",
        String::from_utf8_lossy(&e)
    );

    for id in 1..=3 {
        assert!(
            vault.path().join(format!("seq/MMR-{id}.md")).exists(),
            "interleaved applies must each resolve a unique id; MMR-{id}.md missing"
        );
    }
    assert!(
        !vault.path().join("seq/MMR-4.md").exists(),
        "no extra id should be allocated"
    );
}

// ── Gate shapes: these must run DIRECT even with a live daemon ───────────────

/// `--body-from-stdin` has no wire-faithful stdin analogue, so it is GATED to
/// Direct even with a live daemon. Both the direct and the daemon-home runs must
/// be byte-identical AND the daemon must have served ZERO `vault.new` calls.
#[test]
fn body_from_stdin_runs_direct() {
    let daemon = spawn_ready_daemon_with_log(&[]);
    let seed = base_seed();
    let args = &["a.md", "--body-from-stdin", "--yes"];
    let body = "hello from stdin\n";

    let direct_vault = fresh_vault();
    let routed_vault = fresh_vault();
    write_vault(direct_vault.path(), &seed);
    write_vault(routed_vault.path(), &seed);

    let dcache = TempDir::new().unwrap();
    let dstate = TempDir::new().unwrap();
    let (d_out, d_err, d_code) = run_new_with_stdin(
        dcache.path(),
        dstate.path(),
        direct_vault.path(),
        args,
        body,
    );
    assert_eq!(d_code, 0, "direct --body-from-stdin apply must succeed");

    let (r_out, r_err, r_code) = run_new_with_stdin(
        &daemon.cache_home,
        &daemon.state_home,
        routed_vault.path(),
        args,
        body,
    );

    assert_eq!(
        redact_trace(&r_out),
        redact_trace(&d_out),
        "stdout must match"
    );
    assert_eq!(r_err, d_err, "stderr must match");
    assert_eq!(r_code, d_code, "exit code must match");
    assert_eq!(
        dir_snapshot(direct_vault.path()),
        dir_snapshot(routed_vault.path()),
        "post-state vault must be byte-identical",
    );
    // The gate: the daemon must NEVER have served a vault.new for a stdin body.
    let served = count_served(&daemon.stderr_path, "vault.new");
    assert_eq!(
        served,
        0,
        "--body-from-stdin must force Direct: the daemon served {served} vault.new call(s).\n\
         daemon stderr:\n{}",
        std::fs::read_to_string(&daemon.stderr_path).unwrap_or_default(),
    );
    // Sanity: the stdin body actually landed on disk.
    let created = std::fs::read_to_string(direct_vault.path().join("a.md")).unwrap();
    assert!(
        created.contains("hello from stdin"),
        "stdin body must be written: {created}"
    );
}

/// `--config` / `--no-cache-refresh` force Direct even with a live daemon — the
/// same `routing_forced_direct` guard the read seam applies (the #113 lesson: a
/// live daemon must never serve a call whose flags demand direct semantics). The
/// proof is the served-count: for BOTH flags the daemon must serve ZERO
/// `vault.new` calls, and routed-home output stays byte-identical to direct.
///
/// (Aside: `norn new` currently ignores `--config` entirely — its
/// `preflight_and_plan` loads `load_config(root, None)`, never threading the
/// override — so this shape can only prove the served-count guard, not a
/// behavioral divergence the way `set`'s `--config` case does. The
/// `--no-cache-refresh` shape is in the same served-count boat: a fresh-vault
/// create's frontmatter comes from config, not cache state. Served-count is the
/// load-bearing #113 regression proof for `new`.)
#[test]
fn forced_direct_flags_never_route() {
    let daemon = spawn_ready_daemon_with_log(&[]);
    let seed = base_seed();

    // An alternate config OUTSIDE the vault (exercises the `--config` guard path).
    let alt_config_dir = TempDir::new().unwrap();
    let alt_config = alt_config_dir.path().join("alt-config.yaml");
    std::fs::write(
        &alt_config,
        "validate:\n  rules:\n    - name: any\n      match:\n        path: \"**/*.md\"\n      \
         frontmatter_defaults:\n        type: doc\n",
    )
    .unwrap();
    let alt_config_str = alt_config.to_str().expect("utf8 alt config path");

    let config_args = vec!["c.md", "--yes", "--config", alt_config_str];
    let no_refresh_args = vec!["c.md", "--yes", "--no-cache-refresh"];
    let shapes: Vec<(&str, &[&str])> = vec![
        ("--config alt-config apply", &config_args),
        ("--no-cache-refresh apply", &no_refresh_args),
    ];

    for (name, args) in shapes {
        let direct_vault = fresh_vault();
        let routed_vault = fresh_vault();
        write_vault(direct_vault.path(), &seed);
        write_vault(routed_vault.path(), &seed);

        let dcache = TempDir::new().unwrap();
        let dstate = TempDir::new().unwrap();
        let (d_out, d_err, d_code) =
            run_new(dcache.path(), dstate.path(), direct_vault.path(), args);
        assert_eq!(
            d_code,
            0,
            "[{name}] direct must apply; stderr: {}",
            String::from_utf8_lossy(&d_err)
        );

        let (r_out, r_err, r_code) = run_new(
            &daemon.cache_home,
            &daemon.state_home,
            routed_vault.path(),
            args,
        );
        assert_eq!(
            redact_trace(&r_out),
            redact_trace(&d_out),
            "[{name}] stdout must match direct"
        );
        assert_eq!(r_err, d_err, "[{name}] stderr must match direct");
        assert_eq!(r_code, d_code, "[{name}] exit code must match direct");
        assert_eq!(
            dir_snapshot(direct_vault.path()),
            dir_snapshot(routed_vault.path()),
            "[{name}] the whole post-state vault must be byte-identical",
        );

        // The load-bearing guard: the daemon must NEVER serve a forced-Direct call.
        let served = count_served(&daemon.stderr_path, "vault.new");
        assert_eq!(
            served,
            0,
            "[{name}] --config / --no-cache-refresh must force Direct: the daemon served {served} \
             vault.new call(s).\ndaemon stderr:\n{}",
            std::fs::read_to_string(&daemon.stderr_path).unwrap_or_default(),
        );
    }
}

/// A malformed `--var` is a pure argv error validated CLI-side BEFORE send: it
/// must refuse with the direct path's exact `error:` prose + exit 2 on BOTH the
/// routed and direct paths, and the daemon must have served ZERO `vault.new`
/// calls (the refusal never crossed the wire).
#[test]
fn malformed_var_refuses_pre_send() {
    let daemon = spawn_ready_daemon_with_log(&[]);
    let seed = rule_seed();
    let args = &["--as", "task", "--var", "noequals", "--parents", "--yes"];

    let direct_vault = fresh_vault();
    let routed_vault = fresh_vault();
    write_vault(direct_vault.path(), &seed);
    write_vault(routed_vault.path(), &seed);

    let dcache = TempDir::new().unwrap();
    let dstate = TempDir::new().unwrap();
    let (d_out, d_err, d_code) = run_new(dcache.path(), dstate.path(), direct_vault.path(), args);
    let (r_out, r_err, r_code) = run_new(
        &daemon.cache_home,
        &daemon.state_home,
        routed_vault.path(),
        args,
    );

    assert_eq!(d_code, 2, "direct malformed --var must exit 2");
    assert_eq!(r_code, 2, "routed malformed --var must exit 2");
    assert_eq!(r_out, d_out, "stdout must match (both empty)");
    assert_eq!(
        r_err,
        d_err,
        "stderr must match\nrouted: {:?}\ndirect: {:?}",
        String::from_utf8_lossy(&r_err),
        String::from_utf8_lossy(&d_err),
    );
    assert!(
        String::from_utf8_lossy(&d_err).contains("invalid --var format (expected KEY=VALUE)"),
        "direct stderr must carry the exact --var prose: {:?}",
        String::from_utf8_lossy(&d_err)
    );
    let served = count_served(&daemon.stderr_path, "vault.new");
    assert_eq!(
        served, 0,
        "a pre-send --var refusal must never reach the daemon: served {served}",
    );
}

// ── Contention: routed apply under a held lock is SAFE ───────────────────────

/// Recursively find the first file named `name` under `root`.
fn find_file_named(root: &Path, name: &str) -> Option<std::path::PathBuf> {
    for entry in std::fs::read_dir(root).ok()? {
        let entry = entry.ok()?;
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_file_named(&path, name) {
                return Some(found);
            }
        } else if path.file_name().and_then(|n| n.to_str()) == Some(name) {
            return Some(path);
        }
    }
    None
}

/// A routed apply while the vault's mutation lock is held externally must be SAFE
/// — a non-zero exit and NOTHING written. This is the one case where routed and
/// direct legitimately diverge (routed → `post-send-uncertain`/exit 1 or the
/// daemon's coded lock-timeout refusal, whose prose differs from the direct new
/// arm's hardcoded lock-timeout line), so it pins only the safety, not
/// byte-identity — the same call `serve_set_routing` makes. The debug-only
/// `NORN_MUTATION_LOCK_TIMEOUT_MS` knob keeps the contended acquire at ~150ms.
#[test]
fn routed_apply_under_held_lock_is_safe() {
    use std::os::unix::io::AsRawFd;

    let seed = base_seed();
    let daemon = spawn_ready_daemon_with_log(&[("NORN_MUTATION_LOCK_TIMEOUT_MS", "150")]);
    let vault = fresh_vault();
    write_vault(vault.path(), &seed);
    let apply_args = &["locked.md", "--yes"];

    // 1. A warm-up routed apply materializes the daemon's state dir + lock, then
    //    remove the created doc so a second write would be detectable.
    let (_o, warmup_err, warmup_code) = run_new(
        &daemon.cache_home,
        &daemon.state_home,
        vault.path(),
        apply_args,
    );
    assert_eq!(
        warmup_code,
        0,
        "warm-up routed apply should succeed; stderr: {}",
        String::from_utf8_lossy(&warmup_err)
    );
    std::fs::remove_file(vault.path().join("locked.md")).expect("remove warm-up doc");

    // 2. Hold the daemon's mutation lock exclusively so the next routed apply
    //    must contend and time out.
    let lock_path = find_file_named(&daemon.state_home, ".mutation.lock")
        .expect("the warm-up apply must have created the mutation lock file");
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("open the mutation lock file");
    // SAFETY: a valid open fd; LOCK_EX|LOCK_NB acquires without blocking.
    let rc = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(
        rc,
        0,
        "test setup: could not hold the mutation lock: {}",
        std::io::Error::last_os_error()
    );

    // 3. The routed apply now contends → the daemon times out → the CLI must fail
    //    safely: non-zero exit, and NOTHING written (locked.md absent).
    let (_out, err, code) = run_new(
        &daemon.cache_home,
        &daemon.state_home,
        vault.path(),
        apply_args,
    );
    assert_ne!(
        code,
        0,
        "a routed apply under a held lock must NOT succeed; stderr: {}",
        String::from_utf8_lossy(&err)
    );
    assert!(
        !vault.path().join("locked.md").exists(),
        "a contended routed apply must write NOTHING",
    );

    // The daemon served the contended call (the marker fires before the handler),
    // proving it was the daemon — not a local fall-back — that refused it.
    let served = count_served(&daemon.stderr_path, "vault.new");
    assert!(
        served >= 2,
        "the daemon must have served both the warm-up and the contended apply, got {served}"
    );

    // SAFETY: release the flock via the owning fd before it drops.
    let _ = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_UN) };
    drop(lock_file);
}
