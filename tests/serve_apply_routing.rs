//! End-to-end proof that `norn apply` routes through the warm daemon (NRN-231) —
//! byte-identical to Direct, including a REAL apply on the wire (`create_document`
//! writing a new doc) and the full post-state `dir_snapshot`. Copies the
//! `serve_delete_routing` template.
//!
//! The plan crosses as the PARSED plan (never a path): the CLI reads + parses it
//! client-side (file OR stdin, JSON OR YAML) and ships the re-serialized
//! `MigrationPlan` in the `vault.apply` `plan` argument, so all input shapes route
//! the same way. A schema-version mismatch refuses client-side (served 0), and the
//! daemon's own `mutation-lock-timeout` is reproduced as the CLI-owned pending
//! stash (a dedicated test).

#![cfg(unix)]

#[path = "serve_util/mod.rs"]
mod serve_util;

use serve_util::{count_served, dir_snapshot, norn_bin, spawn_ready_daemon_with_log, write_vault};

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use tempfile::TempDir;

fn fresh_vault() -> TempDir {
    tempfile::Builder::new()
        .prefix("norn-apply-route-vault-")
        .tempdir()
        .expect("vault tempdir")
}

/// Seed: a single witness doc (target of the stale-hash refusal shape).
fn seeded_vault_files() -> Vec<(&'static str, &'static str)> {
    vec![("note.md", "---\ntype: note\n---\nWitness body\n")]
}

/// Canonical absolute vault root (the applier writes relative to `plan.vault_root`,
/// so it must resolve to the real vault dir on both the direct and routed paths).
fn canonical(vault: &Path) -> String {
    std::fs::canonicalize(vault)
        .expect("canonicalize vault")
        .to_str()
        .expect("utf8 vault path")
        .to_string()
}

/// Which plan a shape applies. Each builder templates `vault_root` per vault.
#[derive(Clone, Copy)]
enum PlanKind {
    /// Create a new top-level doc — a clean apply that mutates the vault.
    CreateDoc,
    /// Create a doc in a missing subdir — needs `--parents`.
    CreateSubDoc,
    /// `add_frontmatter` with a wrong `document_hash` → `stale-document-hash`
    /// refusal (writes nothing).
    StaleHash,
    /// A `create_document` whose `new_value` has no `frontmatter` object → the
    /// applier's bare-`anyhow` PRE-WRITE refusal (NRN-231 review F1). Direct
    /// exits 2 with the plain error; the routed path must reproduce that exit-2
    /// refusal, NOT a false post-send-uncertain (exit 1), because the daemon
    /// crosses the pre-write error as a coded, report-shaped refusal.
    MissingFrontmatter,
    /// A structurally valid plan with an unsupported `schema_version` → the
    /// CLIENT-side preflight refusal (must NOT route).
    BadVersion,
}

/// JSON plan text for `kind`, templated to `vault_root`.
fn plan_json(kind: PlanKind, vault_root: &str) -> String {
    match kind {
        PlanKind::CreateDoc => format!(
            r##"{{"schema_version":2,"vault_root":{root},"operations":[{{"kind":"create_document","fields":{{"path":"new.md","new_value":{{"frontmatter":{{"type":"note"}},"body":"# New\n"}}}}}}]}}"##,
            root = serde_json::to_string(vault_root).unwrap()
        ),
        PlanKind::CreateSubDoc => format!(
            r##"{{"schema_version":2,"vault_root":{root},"operations":[{{"kind":"create_document","fields":{{"path":"sub/dir/new.md","new_value":{{"frontmatter":{{"type":"note"}},"body":"# New\n"}}}}}}]}}"##,
            root = serde_json::to_string(vault_root).unwrap()
        ),
        PlanKind::StaleHash => format!(
            r##"{{"schema_version":2,"vault_root":{root},"operations":[{{"kind":"add_frontmatter","fields":{{"path":"note.md","field":"status","new_value":"done","document_hash":"0000000000000000000000000000000000000000000000000000000000000000"}}}}]}}"##,
            root = serde_json::to_string(vault_root).unwrap()
        ),
        PlanKind::MissingFrontmatter => format!(
            r##"{{"schema_version":2,"vault_root":{root},"operations":[{{"kind":"create_document","fields":{{"path":"new.md","new_value":{{"body":"# New\n"}}}}}}]}}"##,
            root = serde_json::to_string(vault_root).unwrap()
        ),
        PlanKind::BadVersion => format!(
            r##"{{"schema_version":99,"vault_root":{root},"operations":[]}}"##,
            root = serde_json::to_string(vault_root).unwrap()
        ),
    }
}

/// YAML equivalent of `CreateDoc`, templated to `vault_root`.
fn plan_yaml_create(vault_root: &str) -> String {
    format!(
        "schema_version: 2\nvault_root: {root}\noperations:\n  - kind: create_document\n    fields:\n      path: new.md\n      new_value:\n        frontmatter:\n          type: note\n        body: \"# New\\n\"\n",
        root = serde_json::to_string(vault_root).unwrap()
    )
}

/// How the plan reaches the CLI.
#[derive(Clone, Copy)]
enum Deliver {
    /// Write to a `.json` file (outside the vault) and pass its path.
    File,
    /// Write to a `.yaml` file (outside the vault) and pass its path.
    YamlFile,
    /// Pipe on stdin and pass `-`.
    Stdin,
}

/// Run `norn apply` against `vault`, delivering `plan_text` per `deliver`.
fn run_apply(
    cache: &Path,
    state: &Path,
    vault: &Path,
    plan_text: &str,
    deliver: Deliver,
    extra_args: &[&str],
) -> (Vec<u8>, Vec<u8>, i32) {
    // Plan files live in their OWN tempdir so they never pollute the vault's
    // `dir_snapshot`.
    let plan_dir = TempDir::new().unwrap();
    let mut cmd = Command::new(norn_bin());
    cmd.env("XDG_CACHE_HOME", cache)
        .env("XDG_STATE_HOME", state)
        .env("NORN_SERVICE_HANDSHAKE_TIMEOUT_MS", "5000")
        .arg("--cwd")
        .arg(vault)
        .arg("apply");

    match deliver {
        Deliver::File => {
            let p = plan_dir.path().join("plan.json");
            std::fs::write(&p, plan_text).unwrap();
            cmd.arg(&p).stdin(Stdio::null());
        }
        Deliver::YamlFile => {
            let p = plan_dir.path().join("plan.yaml");
            std::fs::write(&p, plan_text).unwrap();
            cmd.arg(&p).stdin(Stdio::null());
        }
        Deliver::Stdin => {
            cmd.arg("-").stdin(Stdio::piped());
        }
    }
    cmd.args(extra_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn norn apply");
    if let Deliver::Stdin = deliver {
        child
            .stdin
            .take()
            .expect("stdin pipe")
            .write_all(plan_text.as_bytes())
            .expect("write plan to stdin");
    }
    let out = child.wait_with_output().expect("wait norn apply");
    (out.stdout, out.stderr, out.status.code().unwrap_or(-1))
}

fn redact(bytes: &[u8]) -> String {
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
            let mut s = line.to_string();
            for key in ["trace_id", "vault_root", "plan_hash"] {
                s = redact_json_string_field(&s, key);
            }
            result.push_str(&s);
        }
        result.push_str(nl);
    }
    result
}

fn redact_json_string_field(line: &str, key: &str) -> String {
    let needle = format!("\"{key}\": \"");
    if let Some(start) = line.find(&needle) {
        let after = start + needle.len();
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

/// One row of the byte-identity matrix.
struct Shape {
    name: &'static str,
    kind: PlanKind,
    deliver: Deliver,
    args: Vec<&'static str>,
    expected_exit: i32,
    routes: bool,
}

fn shape(
    name: &'static str,
    kind: PlanKind,
    deliver: Deliver,
    args: Vec<&'static str>,
    expected_exit: i32,
    routes: bool,
) -> Shape {
    Shape {
        name,
        kind,
        deliver,
        args,
        expected_exit,
        routes,
    }
}

fn shape_matrix() -> Vec<Shape> {
    vec![
        shape(
            "dry-run (file)",
            PlanKind::CreateDoc,
            Deliver::File,
            vec!["--dry-run"],
            0,
            true,
        ),
        shape(
            "dry-run (stdin)",
            PlanKind::CreateDoc,
            Deliver::Stdin,
            vec!["--dry-run"],
            0,
            true,
        ),
        shape(
            "json implicit dry-run",
            PlanKind::CreateDoc,
            Deliver::File,
            vec!["--format", "json"],
            0,
            true,
        ),
        shape(
            "non-tty implicit dry-run",
            PlanKind::CreateDoc,
            Deliver::File,
            vec![],
            0,
            true,
        ),
        shape(
            "apply --yes (file)",
            PlanKind::CreateDoc,
            Deliver::File,
            vec!["--yes"],
            0,
            true,
        ),
        shape(
            "apply --yes (stdin)",
            PlanKind::CreateDoc,
            Deliver::Stdin,
            vec!["--yes"],
            0,
            true,
        ),
        shape(
            "apply --yes json (file)",
            PlanKind::CreateDoc,
            Deliver::File,
            vec!["--yes", "--format", "json"],
            0,
            true,
        ),
        shape(
            "apply --yes yaml (file)",
            PlanKind::CreateDoc, // kind ignored for YAML; plan_yaml_create is used
            Deliver::YamlFile,
            vec!["--yes"],
            0,
            true,
        ),
        shape(
            "apply --yes --parents (missing subdir)",
            PlanKind::CreateSubDoc,
            Deliver::File,
            vec!["--yes", "--parents"],
            0,
            true,
        ),
        shape(
            "json refusal (stale-document-hash)",
            PlanKind::StaleHash,
            Deliver::File,
            vec!["--yes", "--format", "json"],
            2,
            true,
        ),
        shape(
            "records refusal (stale-document-hash)",
            PlanKind::StaleHash,
            Deliver::File,
            vec!["--yes"],
            2,
            true,
        ),
        // NRN-231 review F1: a PRE-WRITE bare-`anyhow` refusal (create_document
        // with no frontmatter object) must route byte-identically to Direct's
        // exit-2 refusal — json and records shapes — not surface as a false
        // post-send-uncertain (exit 1). These route (the request reaches the
        // daemon, which crosses the refusal as a coded, report-shaped result).
        shape(
            "json pre-write refusal (missing frontmatter)",
            PlanKind::MissingFrontmatter,
            Deliver::File,
            vec!["--yes", "--format", "json"],
            2,
            true,
        ),
        shape(
            "records pre-write refusal (missing frontmatter)",
            PlanKind::MissingFrontmatter,
            Deliver::File,
            vec!["--yes"],
            2,
            true,
        ),
        shape(
            "schema-version mismatch (gated Direct)",
            PlanKind::BadVersion,
            Deliver::File,
            vec!["--yes"],
            2,
            false,
        ),
    ]
}

fn plan_text_for(kind: PlanKind, deliver: Deliver, vault_root: &str) -> String {
    match deliver {
        Deliver::YamlFile => plan_yaml_create(vault_root),
        _ => plan_json(kind, vault_root),
    }
}

#[test]
fn routed_apply_is_byte_identical_to_direct() {
    let seed = seeded_vault_files();
    let daemon = spawn_ready_daemon_with_log(&[]);

    let mut expected_served = 0usize;
    for Shape {
        name,
        kind,
        deliver,
        args,
        expected_exit,
        routes,
    } in shape_matrix()
    {
        let direct_vault = fresh_vault();
        let routed_vault = fresh_vault();
        write_vault(direct_vault.path(), &seed);
        write_vault(routed_vault.path(), &seed);

        let direct_cache = TempDir::new().unwrap();
        let direct_state = TempDir::new().unwrap();
        let d_plan = plan_text_for(kind, deliver, &canonical(direct_vault.path()));
        let (d_out, d_err, d_code) = run_apply(
            direct_cache.path(),
            direct_state.path(),
            direct_vault.path(),
            &d_plan,
            deliver,
            &args,
        );
        assert_eq!(
            d_code,
            expected_exit,
            "[{name}] direct exit sanity; stderr: {}",
            String::from_utf8_lossy(&d_err)
        );

        let r_plan = plan_text_for(kind, deliver, &canonical(routed_vault.path()));
        let (r_out, r_err, r_code) = run_apply(
            &daemon.cache_home,
            &daemon.state_home,
            routed_vault.path(),
            &r_plan,
            deliver,
            &args,
        );

        assert_eq!(
            redact(&r_out),
            redact(&d_out),
            "[{name}] routed stdout must match direct\nrouted: {:?}\ndirect: {:?}",
            String::from_utf8_lossy(&r_out),
            String::from_utf8_lossy(&d_out),
        );
        assert_eq!(
            redact(&r_err),
            redact(&d_err),
            "[{name}] routed stderr must match direct\nrouted: {:?}\ndirect: {:?}",
            String::from_utf8_lossy(&r_err),
            String::from_utf8_lossy(&d_err),
        );
        assert_eq!(r_code, d_code, "[{name}] routed exit must match direct");

        assert_eq!(
            dir_snapshot(direct_vault.path()),
            dir_snapshot(routed_vault.path()),
            "[{name}] the whole post-state vault must be byte-identical",
        );

        if routes {
            expected_served += 1;
        }
        let served = count_served(&daemon.stderr_path, "vault.apply");
        assert_eq!(
            served,
            expected_served,
            "[{name}] served count must be {expected_served}, got {served}\n{}",
            std::fs::read_to_string(&daemon.stderr_path).unwrap_or_default(),
        );
    }
}

/// `--out <file>`: the report is written to a file (stdout silent), byte-identical
/// between routed and direct (report bodies compared after redaction).
#[test]
fn routed_out_report_is_byte_identical() {
    let seed = seeded_vault_files();
    let daemon = spawn_ready_daemon_with_log(&[]);

    let direct_vault = fresh_vault();
    let routed_vault = fresh_vault();
    write_vault(direct_vault.path(), &seed);
    write_vault(routed_vault.path(), &seed);

    let d_out_dir = TempDir::new().unwrap();
    let d_out_file = d_out_dir.path().join("report.json");
    let r_out_dir = TempDir::new().unwrap();
    let r_out_file = r_out_dir.path().join("report.json");

    let direct_cache = TempDir::new().unwrap();
    let direct_state = TempDir::new().unwrap();
    let d_plan = plan_json(PlanKind::CreateDoc, &canonical(direct_vault.path()));
    let (d_stdout, d_err, d_code) = run_apply(
        direct_cache.path(),
        direct_state.path(),
        direct_vault.path(),
        &d_plan,
        Deliver::File,
        &["--yes", "--out", d_out_file.to_str().unwrap()],
    );
    let r_plan = plan_json(PlanKind::CreateDoc, &canonical(routed_vault.path()));
    let (r_stdout, r_err, r_code) = run_apply(
        &daemon.cache_home,
        &daemon.state_home,
        routed_vault.path(),
        &r_plan,
        Deliver::File,
        &["--yes", "--out", r_out_file.to_str().unwrap()],
    );

    assert_eq!(
        d_code,
        0,
        "direct --out exit; stderr: {}",
        String::from_utf8_lossy(&d_err)
    );
    assert_eq!(
        r_code,
        0,
        "routed --out exit; stderr: {}",
        String::from_utf8_lossy(&r_err)
    );
    // stdout is silent with --out on both paths.
    assert!(d_stdout.is_empty(), "direct --out silences stdout");
    assert!(r_stdout.is_empty(), "routed --out silences stdout");

    let d_report = std::fs::read(&d_out_file).expect("direct wrote report");
    let r_report = std::fs::read(&r_out_file).expect("routed wrote report");
    assert_eq!(
        redact(&r_report),
        redact(&d_report),
        "routed --out report must match direct\nrouted: {:?}\ndirect: {:?}",
        String::from_utf8_lossy(&r_report),
        String::from_utf8_lossy(&d_report),
    );
    assert_eq!(
        dir_snapshot(direct_vault.path()),
        dir_snapshot(routed_vault.path()),
        "--out apply post-state must be byte-identical",
    );

    let served = count_served(&daemon.stderr_path, "vault.apply");
    assert_eq!(
        served, 1,
        "the --out apply must route once, served {served}"
    );
}

#[test]
fn no_daemon_runs_direct() {
    let seed = seeded_vault_files();
    for Shape {
        name,
        kind,
        deliver,
        args,
        expected_exit,
        routes: _,
    } in shape_matrix()
    {
        let vault = fresh_vault();
        write_vault(vault.path(), &seed);
        let cache = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let plan = plan_text_for(kind, deliver, &canonical(vault.path()));
        let (_o, err, code) = run_apply(
            cache.path(),
            state.path(),
            vault.path(),
            &plan,
            deliver,
            &args,
        );
        assert_eq!(
            code,
            expected_exit,
            "[{name}] apply should exit {expected_exit} with no daemon, got {code}; stderr: {}",
            String::from_utf8_lossy(&err)
        );
    }
}

/// `--config` / `--no-cache-refresh` force Direct: served stays 0, output identical.
#[test]
fn forced_direct_flags_never_route() {
    let seed = seeded_vault_files();
    let daemon = spawn_ready_daemon_with_log(&[]);

    let alt_config_dir = TempDir::new().unwrap();
    let alt_config = alt_config_dir.path().join("alt.yaml");
    std::fs::write(&alt_config, "validate:\n  rules: []\n").unwrap();
    let alt_config_str = alt_config.to_str().unwrap().to_string();

    let shapes: Vec<(&str, Vec<String>)> = vec![
        (
            "--config apply",
            vec!["--yes".into(), "--config".into(), alt_config_str.clone()],
        ),
        (
            "--no-cache-refresh apply",
            vec!["--yes".into(), "--no-cache-refresh".into()],
        ),
    ];

    for (name, args) in shapes {
        let direct_vault = fresh_vault();
        let routed_vault = fresh_vault();
        write_vault(direct_vault.path(), &seed);
        write_vault(routed_vault.path(), &seed);
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        let direct_cache = TempDir::new().unwrap();
        let direct_state = TempDir::new().unwrap();
        let d_plan = plan_json(PlanKind::CreateDoc, &canonical(direct_vault.path()));
        let (d_out, d_err, d_code) = run_apply(
            direct_cache.path(),
            direct_state.path(),
            direct_vault.path(),
            &d_plan,
            Deliver::File,
            &arg_refs,
        );
        let r_plan = plan_json(PlanKind::CreateDoc, &canonical(routed_vault.path()));
        let (r_out, r_err, r_code) = run_apply(
            &daemon.cache_home,
            &daemon.state_home,
            routed_vault.path(),
            &r_plan,
            Deliver::File,
            &arg_refs,
        );
        assert_eq!(redact(&r_out), redact(&d_out), "[{name}] stdout must match");
        assert_eq!(redact(&r_err), redact(&d_err), "[{name}] stderr must match");
        assert_eq!(r_code, d_code, "[{name}] exit must match");
        assert_eq!(
            dir_snapshot(direct_vault.path()),
            dir_snapshot(routed_vault.path()),
            "[{name}] post-state vault must be byte-identical",
        );
        let served = count_served(&daemon.stderr_path, "vault.apply");
        assert_eq!(
            served, 0,
            "[{name}] forced-Direct must never route; served {served}"
        );
    }
}

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

/// Lock-timeout stash parity (design point 5): a routed STDIN apply under a held
/// vault mutation lock reproduces the direct arm's stash branch byte-identically —
/// stash the RAW plan under `<state_dir>/pending/`, print the `retry with:` hint,
/// exit 2, and write NOTHING to the vault. The daemon's own `mutation-lock-timeout`
/// refusal crosses back and the CLI (not the generic refusal renderer) owns the
/// stash. Compared against a Direct twin under its own held client lock.
#[test]
fn routed_stdin_apply_under_held_lock_stashes_and_exits_2() {
    let seed = seeded_vault_files();
    let daemon = spawn_ready_daemon_with_log(&[("NORN_MUTATION_LOCK_TIMEOUT_MS", "150")]);
    let routed_vault = fresh_vault();
    write_vault(routed_vault.path(), &seed);

    // Warm-up routed apply to create the daemon-side mutation lock file.
    let warm_plan = plan_json(PlanKind::CreateDoc, &canonical(routed_vault.path()));
    let (_o, warm_err, warm_code) = run_apply(
        &daemon.cache_home,
        &daemon.state_home,
        routed_vault.path(),
        &warm_plan,
        Deliver::Stdin,
        &["--yes"],
    );
    assert_eq!(
        warm_code,
        0,
        "warm-up routed apply should succeed; stderr: {}",
        String::from_utf8_lossy(&warm_err)
    );
    std::fs::remove_file(routed_vault.path().join("new.md")).ok(); // reset

    // Hold the daemon's per-vault mutation lock so the next routed apply times out.
    let lock_path = find_file_named(Path::new(&daemon.state_home), ".mutation.lock")
        .expect("warm-up apply must have created the mutation lock");
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("open lock file");
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(rc, 0, "test setup: could not hold the mutation lock");

    let stdin_plan = plan_json(PlanKind::CreateDoc, &canonical(routed_vault.path()));
    let (r_out, r_err, r_code) = run_apply(
        &daemon.cache_home,
        &daemon.state_home,
        routed_vault.path(),
        &stdin_plan,
        Deliver::Stdin,
        &["--yes"],
    );
    let _ = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_UN) };
    drop(lock_file);

    assert_eq!(
        r_code,
        2,
        "a routed stdin apply under a held lock must exit 2; stderr: {}",
        String::from_utf8_lossy(&r_err)
    );
    assert!(r_out.is_empty(), "no report on a lock-timeout stash");
    assert!(
        !routed_vault.path().join("new.md").exists(),
        "a contended routed apply must write nothing"
    );

    let err_s = String::from_utf8_lossy(&r_err);
    assert!(
        err_s.contains(
            "error: another norn mutation is in progress against this vault (timed out after 5 s)"
        ),
        "routed lock-timeout must print the direct arm's hardcoded line; got: {err_s}"
    );
    assert!(
        err_s.contains("retry with: norn apply "),
        "a stdin plan must be stashed with a retry hint; got: {err_s}"
    );

    // The stashed plan exists under the ROUTED state dir and its retry hint points
    // at it, holding the RAW stdin bytes.
    let stashed = find_pending_plan(Path::new(&daemon.state_home))
        .expect("a stdin lock-timeout must stash a pending plan");
    assert!(
        err_s.contains(stashed.to_str().unwrap()),
        "the retry hint must name the stashed plan {stashed:?}; got: {err_s}"
    );
    assert_eq!(
        std::fs::read_to_string(&stashed).unwrap(),
        stdin_plan,
        "the stashed plan must be the RAW stdin bytes"
    );

    // Compare against a Direct twin under its OWN held client lock: byte-identical
    // after redacting the (state-dir-specific) pending path.
    let direct_vault = fresh_vault();
    write_vault(direct_vault.path(), &seed);
    let direct_cache = TempDir::new().unwrap();
    let direct_state = TempDir::new().unwrap();
    // Warm-up direct apply to create the client-side lock file.
    let d_warm = plan_json(PlanKind::CreateDoc, &canonical(direct_vault.path()));
    let (_o, _e, d_warm_code) = run_apply(
        direct_cache.path(),
        direct_state.path(),
        direct_vault.path(),
        &d_warm,
        Deliver::Stdin,
        &["--yes"],
    );
    assert_eq!(d_warm_code, 0, "direct warm-up apply should succeed");
    std::fs::remove_file(direct_vault.path().join("new.md")).ok();
    let d_lock_path = find_file_named(direct_state.path(), ".mutation.lock")
        .expect("direct warm-up must have created the mutation lock");
    let d_lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&d_lock_path)
        .expect("open direct lock file");
    let rc = unsafe { libc::flock(d_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(rc, 0, "test setup: could not hold the direct mutation lock");
    let d_plan = plan_json(PlanKind::CreateDoc, &canonical(direct_vault.path()));
    let (_d_out, d_err, d_code) = {
        // The direct client also needs the short timeout to time out fast.
        let plan_dir = TempDir::new().unwrap();
        let _ = plan_dir;
        let mut cmd = Command::new(norn_bin());
        cmd.env("XDG_CACHE_HOME", direct_cache.path())
            .env("XDG_STATE_HOME", direct_state.path())
            .env("NORN_MUTATION_LOCK_TIMEOUT_MS", "150")
            .env("NORN_SERVICE_HANDSHAKE_TIMEOUT_MS", "5000")
            .arg("--cwd")
            .arg(direct_vault.path())
            .arg("apply")
            .arg("-")
            .arg("--yes")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn direct apply");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(d_plan.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        (out.stdout, out.stderr, out.status.code().unwrap_or(-1))
    };
    let _ = unsafe { libc::flock(d_lock.as_raw_fd(), libc::LOCK_UN) };
    drop(d_lock);
    assert_eq!(d_code, 2, "direct stdin apply under held lock must exit 2");

    // Redact the pending path (state-dir specific) before comparing.
    let d_err_red = redact_pending_path(&String::from_utf8_lossy(&d_err));
    let r_err_red = redact_pending_path(&err_s);
    assert_eq!(
        r_err_red, d_err_red,
        "routed stash stderr must match direct after pending-path redaction"
    );
}

/// Find the single stashed pending plan under a state dir, if any.
fn find_pending_plan(state_dir: &Path) -> Option<std::path::PathBuf> {
    fn walk(dir: &Path) -> Option<std::path::PathBuf> {
        for entry in std::fs::read_dir(dir).ok()? {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.is_dir() {
                if let Some(f) = walk(&path) {
                    return Some(f);
                }
            } else if path
                .to_str()
                .is_some_and(|s| s.contains("/pending/") && s.ends_with(".plan.json"))
            {
                return Some(path);
            }
        }
        None
    }
    walk(state_dir)
}

/// Replace the absolute pending path in a `retry with:` hint with a placeholder,
/// so a routed vs direct stderr comparison is insensitive to the (per-run) state
/// dir prefix.
fn redact_pending_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for segment in s.split_inclusive('\n') {
        if let Some(rest) = segment.strip_prefix("retry with: norn apply ") {
            out.push_str("retry with: norn apply <pending>");
            if rest.ends_with('\n') {
                out.push('\n');
            }
        } else {
            out.push_str(segment);
        }
    }
    out
}
