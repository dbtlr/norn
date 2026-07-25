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
fn bare_find_prints_help_and_exits_two() {
    // `find` is ported (NRN-346); a bare invocation with no predicate and no
    // `--all` is the help gate — it prints the find help to stderr and exits 2
    // (a full-vault dump is almost always a mistake), never summoning an owner.
    let out = norn().arg("find").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "stdout must stay empty");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("find") && !stderr.is_empty(),
        "expected the find help on stderr, got: {stderr:?}"
    );
}

#[test]
fn edit_cli_side_refusal_exits_two_with_error_line() {
    // `edit` now dispatches for real (NRN-379). A CLI-side op-resolution failure
    // (an empty ops array) is refused BEFORE any owner summon, rendering the
    // format-independent edit refusal surface: `error: <message>` on stderr,
    // exit 2, nothing on stdout — exercised here without touching the network.
    let out = norn()
        .args(["edit", "a.md", "--edits-json", "[]"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "a refusal writes nothing to stdout");
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        "error: edits array is empty\n"
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
    // NRN-370: the `vault` verbs now fold their ConfigError through the same
    // routed diagnostic constructor the read verbs use, so `UnknownName` carries
    // its recovery hint here too (the one deliberate behavior delta).
    assert_eq!(
        stderr_of(&out),
        "norn: no vault named \"ghost\" is registered\n\
         hint: run `norn vault list` to see registered vault names\n"
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

/// NRN-360: a present-but-invalid `.norn/config.yaml` surfaces a config error
/// on the user-error path — exit 1 and a `norn: invalid config <path>: …`
/// diagnostic. The `norn:` prefix is the display layer's convention (NRN-361
/// owns the prefix question). This is the only `cli` test that drives the
/// real bin end-to-end through a summon, so it pins the whole CLI surface the
/// owner/client suites can each only pin in part.
#[cfg(unix)]
#[test]
fn invalid_config_exits_one_with_the_config_diagnostic() {
    use std::time::{SystemTime, UNIX_EPOCH};

    // A vault whose config is present but schema-invalid (unknown top field).
    let vault = tempfile::tempdir().unwrap();
    let norn_dir = vault.path().join(".norn");
    std::fs::create_dir_all(&norn_dir).unwrap();
    std::fs::write(norn_dir.join("config.yaml"), "bogus: true\n").unwrap();
    std::fs::write(vault.path().join("a.md"), "---\ntype: note\n---\nbody\n").unwrap();

    // A SHORT, unique, isolated runtime dir: the summoned owner's Unix socket
    // lives under it and must stay within the ~104-byte `sun_path` limit (a
    // TempDir under the system temp is too long on some platforms), and the
    // isolation keeps this off the developer's real runtime dir / owners.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let runtime_dir = std::path::PathBuf::from(format!("/tmp/nrn360-{}", nanos % 100_000_000));
    let _ = std::fs::remove_dir_all(&runtime_dir);
    // Isolate the central-config home too, so resolution never reads the dev's.
    let cfg_home = tempfile::tempdir().unwrap();

    let out = norn_summoning(&runtime_dir, cfg_home.path())
        .arg("-C")
        .arg(vault.path())
        .args(["find", "--all"])
        .output()
        .unwrap();

    let _ = std::fs::remove_dir_all(&runtime_dir); // best-effort cleanup

    let stderr = stderr_of(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "a bad config must exit 1; stderr was: {stderr:?}"
    );
    // The config-error message body under the `norn:` diagnostic prefix.
    // Path-tail + serde detail are asserted (not the canonicalized absolute
    // prefix, which varies with the temp dir's symlink spelling).
    assert!(
        stderr.contains("norn: invalid config "),
        "expected the `norn: invalid config …` diagnostic, got: {stderr:?}"
    );
    assert!(
        stderr.contains(".norn/config.yaml: unknown field `bogus`"),
        "expected the config path + serde detail, got: {stderr:?}"
    );
}

/// A unique, short, isolated runtime dir for a summon-driven test (keeps the
/// owner's Unix socket inside `sun_path`'s ~104-byte limit and off the dev's
/// real runtime dir). `tag` disambiguates concurrent tests in this file.
#[cfg(unix)]
fn isolated_runtime_dir(tag: &str) -> std::path::PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::path::PathBuf::from(format!("/tmp/nrn367-{tag}-{}", nanos % 100_000_000));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// The owner idle TTL a summon-driven test runs its owners under. Short, because
/// a test removes its runtime dir the moment it finishes while the detached
/// owners it summoned are still alive: at the 120s production default those
/// owners squat a deleted directory for two minutes after the suite exits.
#[cfg(unix)]
const TEST_OWNER_TTL_SECS: &str = "5";

/// A `norn` invocation that will summon an owner: isolated runtime dir, isolated
/// central-config home, and [`TEST_OWNER_TTL_SECS`] so anything left behind
/// reaps promptly.
#[cfg(unix)]
fn norn_summoning(runtime_dir: &Path, cfg_home: &Path) -> Command {
    let mut cmd = norn_cfg(cfg_home);
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env("NORN_EPHEMERAL_TTL_SECS", TEST_OWNER_TTL_SECS);
    cmd
}

/// Are file permission bits actually enforced for this process? A process that
/// bypasses DAC — euid 0, or `CAP_DAC_OVERRIDE` — reads a `0o000` file anyway,
/// so permission bits are advisory to it and a test encoding "`0o000` means
/// unreadable" would invert rather than skip. Probed behaviorally, not by euid,
/// so it also covers the capability case and a filesystem mounted without
/// permission enforcement.
#[cfg(unix)]
fn permission_bits_enforced() -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Ok(probe) = tempfile::NamedTempFile::new() else {
        return true;
    };
    if std::fs::set_permissions(probe.path(), std::fs::Permissions::from_mode(0o000)).is_err() {
        return true;
    }
    std::fs::read(probe.path()).is_err()
}

/// Every `*.sock` name currently bound under `runtime_dir`'s norn subdir. Called
/// after each invocation and unioned across a run, this counts the DISTINCT
/// owners a sequence of commands addressed — robust to an owner reaping (and
/// deleting its socket) between two invocations, which a single end-of-test
/// listing would miss.
#[cfg(unix)]
fn socket_names(runtime_dir: &Path) -> std::collections::BTreeSet<String> {
    let Ok(entries) = std::fs::read_dir(runtime_dir.join("norn")) else {
        return std::collections::BTreeSet::new();
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".sock"))
        .collect()
}

/// A single-document vault seeded with `title` / `status` frontmatter, returned
/// as `(guard, vault_root)`. The vault is a NON-hidden `vault/` subdirectory of
/// the tempdir: `tempfile::tempdir()` names its dir `.tmpXXXX` (dot-prefixed),
/// and the graph walk skips hidden directories, so warming a dot-prefixed root
/// directly would index zero documents. The subdir keeps the walked root
/// non-hidden while the parent tempdir still auto-cleans.
#[cfg(unix)]
fn seeded_vault() -> (tempfile::TempDir, std::path::PathBuf) {
    let guard = tempfile::tempdir().unwrap();
    let vault = guard.path().join("vault");
    std::fs::create_dir(&vault).unwrap();
    std::fs::write(
        vault.join("a.md"),
        "---\ntype: note\ntitle: Hello\nstatus: active\n---\nbody\n",
    )
    .unwrap();
    (guard, vault)
}

/// NRN-367: an unknown dynamic field (`--titel foo`, a typo of the `title`
/// field) rejects end-to-end with a `norn:` headline naming the field plus a
/// did-you-mean `hint:` line, exit 1, and a byte-empty stdout — instead of the
/// pre-gate behavior of silently desugaring to `--eq titel:foo` and returning an
/// empty result set at exit 0. Drives the real bin through a summon so the whole
/// owner-side-gate → wire → CLI-diagnostic path is exercised.
#[cfg(unix)]
#[test]
fn unknown_dynamic_field_rejects_with_did_you_mean() {
    let (_guard, vault) = seeded_vault();
    let runtime_dir = isolated_runtime_dir("unknown");
    let cfg_home = tempfile::tempdir().unwrap();

    let out = norn_summoning(&runtime_dir, cfg_home.path())
        .arg("-C")
        .arg(&vault)
        .args(["find", "--titel", "foo"])
        .output()
        .unwrap();

    let _ = std::fs::remove_dir_all(&runtime_dir);

    let stderr = stderr_of(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "an unknown dynamic field must exit 1; stderr was: {stderr:?}"
    );
    assert!(
        out.stdout.is_empty(),
        "stdout must stay byte-empty on a rejection, got: {:?}",
        stdout_of(&out)
    );
    assert!(
        stderr.contains("norn: unknown field `titel`"),
        "expected the `norn:`-prefixed headline naming the field, got: {stderr:?}"
    );
    assert!(
        stderr.contains("hint:") && stderr.contains("`title`"),
        "expected a did-you-mean `hint:` line pointing at `title`, got: {stderr:?}"
    );
}

/// NRN-367 correctness guard: a VALID dynamic field that simply matches zero
/// documents must NOT be gated — it returns an empty set at exit 0, exactly as
/// before. `--status backlog` is a known (observed) field with a value no doc
/// carries, so the gate passes and the query runs to an empty result.
#[cfg(unix)]
#[test]
fn valid_dynamic_field_with_zero_matches_stays_empty_exit_zero() {
    let (_guard, vault) = seeded_vault();
    let runtime_dir = isolated_runtime_dir("zeromatch");
    let cfg_home = tempfile::tempdir().unwrap();

    let out = norn_summoning(&runtime_dir, cfg_home.path())
        .arg("-C")
        .arg(&vault)
        .args(["find", "--status", "backlog", "--format", "paths"])
        .output()
        .unwrap();

    let _ = std::fs::remove_dir_all(&runtime_dir);

    let stderr = stderr_of(&out);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a valid field matching nothing must exit 0; stderr was: {stderr:?}"
    );
    assert!(
        out.stdout.is_empty(),
        "a zero-match query has empty stdout, got: {:?}",
        stdout_of(&out)
    );
    assert!(
        !stderr.contains("unknown field"),
        "a known field must not be gated as unknown, got: {stderr:?}"
    );
}

/// NRN-414: a missing `-C` vault root is refused by the instant client-side
/// precheck BEFORE any owner is summoned — no connect budget burned, no owner
/// spawned. This pins the "no-summon" half of that contract end-to-end: not
/// just the exit code/diagnostic (covered by the `norn-cli` `routed` unit
/// tests and the `norn-parity` `err-missing-vault-root-zoo` case), but that
/// summoning genuinely never happens. If an owner had been summoned, its
/// control socket and per-owner db dir would land directly under
/// `$XDG_RUNTIME_DIR/norn` (see `norn-owner`'s `vault_root_error_surface`
/// test for that lifecycle) — that dir is created lazily at first summon, so
/// its total absence here is the discriminator.
#[cfg(unix)]
#[test]
fn missing_vault_root_precheck_refuses_with_no_owner_summoned() {
    let runtime_dir = isolated_runtime_dir("nrn414-missing-root");
    let cfg_home = tempfile::tempdir().unwrap();
    let missing_root = runtime_dir.join("nrn414-does-not-exist");

    let out = norn()
        .arg("-C")
        .arg(&missing_root)
        .args(["find", "--all"])
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("NORN_CONFIG_DIR", cfg_home.path())
        .output()
        .unwrap();

    let norn_runtime_subdir = runtime_dir.join("norn");
    let leftover: Vec<_> = std::fs::read_dir(&norn_runtime_subdir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.file_name())
                .collect()
        })
        .unwrap_or_default();
    let _ = std::fs::remove_dir_all(&runtime_dir);

    let stderr = stderr_of(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "a missing -C root must exit 1; stderr was: {stderr:?}"
    );
    assert!(
        out.stdout.is_empty(),
        "stdout must stay byte-empty on the precheck refusal, got: {:?}",
        stdout_of(&out)
    );
    assert!(
        stderr.contains("norn: vault root does not exist"),
        "expected the precheck's diagnostic, got: {stderr:?}"
    );
    assert!(
        stderr.contains("(from -C)"),
        "expected the precheck to name the -C via, got: {stderr:?}"
    );
    // No-summon assertion: the owner runtime subdir (created lazily at first
    // summon) must not exist at all — no control socket, no db dir, nothing.
    assert!(
        !norn_runtime_subdir.exists(),
        "the precheck refusal must summon no owner, but found runtime artifacts: {leftover:?}"
    );
}

/// NRN-475: an owner reads `.norn/config.yaml` ONCE, at warm-up, so a socket
/// keyed by the vault root alone kept every later invocation running under the
/// PREVIOUS schema for the rest of the idle TTL (120s by default) — a write that
/// the edited config forbids applied at exit 0. The socket is now keyed by the
/// config's content identity too, so the invocation that follows an edit
/// summons an owner holding the new schema and the constraint is enforced.
///
/// Repro shape: warm an owner under a config with NO `allowed_values`, add the
/// constraint, then write a value it forbids.
#[cfg(unix)]
#[test]
fn config_edited_after_warm_up_is_enforced_by_the_next_invocation() {
    let guard = tempfile::tempdir().unwrap();
    // A non-hidden subdir: the graph walk skips dot-prefixed dirs, and
    // `tempfile::tempdir()` names its dir `.tmpXXXX`.
    let vault = guard.path().join("vault");
    std::fs::create_dir_all(vault.join(".norn")).unwrap();
    std::fs::write(
        vault.join("a.md"),
        "---\ntype: note\ntitle: A\nstatus: backlog\n---\nbody\n",
    )
    .unwrap();
    let config = vault.join(".norn").join("config.yaml");
    std::fs::write(
        &config,
        "validate:\n  rules:\n    - name: notes\n      field_types:\n        status:\n          type: string\n",
    )
    .unwrap();

    let runtime_dir = isolated_runtime_dir("cfgedit");
    let cfg_home = tempfile::tempdir().unwrap();

    // Warm an owner under the permissive config.
    let warm = norn_summoning(&runtime_dir, cfg_home.path())
        .arg("-C")
        .arg(&vault)
        .args(["count"])
        .output()
        .unwrap();
    assert_eq!(
        warm.status.code(),
        Some(0),
        "warming must succeed; stderr was: {:?}",
        stderr_of(&warm)
    );

    // Constrain `status`. The warm owner still holds the permissive schema.
    std::fs::write(
        &config,
        "validate:\n  rules:\n    - name: notes\n      field_types:\n        status:\n          type: string\n      allowed_values:\n        status:\n          - backlog\n          - done\n",
    )
    .unwrap();

    let out = norn_summoning(&runtime_dir, cfg_home.path())
        .arg("-C")
        .arg(&vault)
        .args(["set", "a.md", "--field", "status=bogus", "--yes"])
        .output()
        .unwrap();

    let _ = std::fs::remove_dir_all(&runtime_dir);

    let stderr = stderr_of(&out);
    assert_eq!(
        out.status.code(),
        Some(2),
        "a schema refusal is exit 2 (docs/errors.md); stderr was: {stderr:?}"
    );
    assert!(
        stderr.contains("is not allowed for 'status'"),
        "expected the allowed-values refusal, got: {stderr:?}"
    );
    assert_eq!(
        std::fs::read_to_string(vault.join("a.md")).unwrap(),
        "---\ntype: note\ntitle: A\nstatus: backlog\n---\nbody\n",
        "a refused write must leave the document untouched"
    );
}

/// NRN-475 companion: a config CREATED after an owner warmed (the fresh-vault
/// shape — first command, then write the config) was invisible for the owner's
/// whole idle TTL, so an invalid config produced clean exit-0 reads. Creating
/// the file changes the config identity, so the next invocation summons an owner
/// that reads it and surfaces the load error.
#[cfg(unix)]
#[test]
fn config_created_after_warm_up_surfaces_its_load_error() {
    let guard = tempfile::tempdir().unwrap();
    let vault = guard.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::write(vault.join("a.md"), "---\ntype: note\ntitle: A\n---\nbody\n").unwrap();

    let runtime_dir = isolated_runtime_dir("cfgnew");
    let cfg_home = tempfile::tempdir().unwrap();

    // Warm an owner with NO config file present.
    let warm = norn_summoning(&runtime_dir, cfg_home.path())
        .arg("-C")
        .arg(&vault)
        .args(["count"])
        .output()
        .unwrap();
    assert_eq!(
        warm.status.code(),
        Some(0),
        "warming must succeed; stderr was: {:?}",
        stderr_of(&warm)
    );

    // A rule whose own default its own `allowed_values` forbids — rejected at
    // config load, not per document.
    std::fs::create_dir_all(vault.join(".norn")).unwrap();
    std::fs::write(
        vault.join(".norn").join("config.yaml"),
        "validate:\n  rules:\n    - name: notes\n      allowed_values:\n        status:\n          - backlog\n          - done\n      frontmatter_defaults:\n        status: active\n",
    )
    .unwrap();

    let out = norn_summoning(&runtime_dir, cfg_home.path())
        .arg("-C")
        .arg(&vault)
        .args(["validate"])
        .output()
        .unwrap();

    let _ = std::fs::remove_dir_all(&runtime_dir);

    let stderr = stderr_of(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "an invalid config must exit 1; stderr was: {stderr:?}"
    );
    assert!(
        stderr.contains("norn: invalid config "),
        "expected the config-load diagnostic, got: {stderr:?}"
    );
    assert!(
        stderr.contains("is not in this rule's allowed_values"),
        "expected the self-contradicting-default detail, got: {stderr:?}"
    );
}

/// NRN-475 / NRN-487: the owner is addressed by (vault root, build, config
/// identity), so every addressing via for one registered root MUST derive the
/// same config — otherwise the four vias split into two owners holding two
/// schemas over one vault, each with its own writer lock.
///
/// `-C <root>` and `NORN_ROOT` resolve with `vault: None` even for a registered
/// root, so reading the `[vaults.<name>].config` override off the resolved entry
/// gave those two vias the vault's DEFAULT config path while `--vault` and the
/// cwd via got the override. The override is now reverse-looked-up from the
/// canonical root, independent of addressing.
///
/// The assertion is the observable one: one registered root whose override
/// constrains `status`, four vias, one socket name and four identical refusals.
#[cfg(unix)]
#[test]
fn all_addressing_vias_for_one_registered_root_derive_one_owner() {
    let guard = tempfile::tempdir().unwrap();
    let vault = guard.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::write(
        vault.join("a.md"),
        "---\ntype: note\ntitle: A\nstatus: backlog\n---\nbody\n",
    )
    .unwrap();
    // The schema lives OUTSIDE the vault, reachable only through the
    // registration — so a via that misses the override sees no config at all.
    let override_config = guard.path().join("schema.yaml");
    std::fs::write(
        &override_config,
        "validate:\n  rules:\n    - name: notes\n      allowed_values:\n        status:\n          - backlog\n          - done\n",
    )
    .unwrap();

    let runtime_dir = isolated_runtime_dir("vias");
    let cfg_home = tempfile::tempdir().unwrap();

    let out = norn_summoning(&runtime_dir, cfg_home.path())
        .args(["vault", "register", "reg"])
        .arg(&vault)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "register: {:?}",
        stderr_of(&out)
    );
    let out = norn_summoning(&runtime_dir, cfg_home.path())
        .args(["vault", "set", "reg", "--config"])
        .arg(&override_config)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "set: {:?}", stderr_of(&out));

    let forbidden = ["set", "a.md", "--field", "status=bogus", "--yes"];
    let mut sockets = std::collections::BTreeSet::new();
    let mut run = |label: &str, cmd: &mut Command| {
        let out = cmd.output().unwrap();
        let stderr = stderr_of(&out);
        assert_eq!(
            out.status.code(),
            Some(2),
            "via {label}: a schema refusal is exit 2; stderr was: {stderr:?}"
        );
        assert!(
            stderr.contains("is not allowed for 'status'"),
            "via {label}: expected the override's constraint, got: {stderr:?}"
        );
        sockets.extend(socket_names(&runtime_dir));
    };

    run(
        "--vault",
        norn_summoning(&runtime_dir, cfg_home.path())
            .args(["--vault", "reg"])
            .args(forbidden),
    );
    run(
        "-C",
        norn_summoning(&runtime_dir, cfg_home.path())
            .arg("-C")
            .arg(&vault)
            .args(forbidden),
    );
    run(
        "NORN_ROOT",
        norn_summoning(&runtime_dir, cfg_home.path())
            .env("NORN_ROOT", &vault)
            .args(forbidden),
    );
    run(
        "cwd",
        norn_summoning(&runtime_dir, cfg_home.path())
            .current_dir(&vault)
            .args(forbidden),
    );

    let _ = std::fs::remove_dir_all(&runtime_dir);

    assert_eq!(
        sockets.len(),
        1,
        "all four vias must address ONE owner, saw sockets: {sockets:?}"
    );
    assert_eq!(
        std::fs::read_to_string(vault.join("a.md")).unwrap(),
        "---\ntype: note\ntitle: A\nstatus: backlog\n---\nbody\n",
        "no via may have applied the forbidden value"
    );
}

/// NRN-475: a config that is PRESENT but the owner cannot READ is a hard error
/// (`failed to read config <path>`, exit 1), not the run-under-defaults case —
/// so its identity must differ from the no-config identity, or a warm
/// defaults-owner keeps answering exit 0 where a cold owner exits 1. Covers the
/// three unreadable shapes: a permissions denial, a directory where the file
/// belongs, and a registered override pointing at nothing.
#[cfg(unix)]
#[test]
fn a_config_that_appears_after_warm_up_but_cannot_be_read_is_not_served_as_absent() {
    use std::os::unix::fs::PermissionsExt;

    let guard = tempfile::tempdir().unwrap();
    let vault = guard.path().join("vault");
    std::fs::create_dir_all(vault.join(".norn")).unwrap();
    std::fs::write(vault.join("a.md"), "---\ntype: note\ntitle: A\n---\nbody\n").unwrap();

    let runtime_dir = isolated_runtime_dir("unreadable");
    let cfg_home = tempfile::tempdir().unwrap();

    let count = |label: &str| {
        norn_summoning(&runtime_dir, cfg_home.path())
            .arg("-C")
            .arg(&vault)
            .args(["count"])
            .output()
            .unwrap_or_else(|e| panic!("{label}: {e}"))
    };

    // Warm an owner with NO config file: it serves under defaults, exit 0.
    let warm = count("warm");
    assert_eq!(
        warm.status.code(),
        Some(0),
        "warming must succeed; stderr was: {:?}",
        stderr_of(&warm)
    );

    // (1) A chmod-000 config appears. The warm owner runs under defaults; a
    // cold one would refuse. Only meaningful where the bits bind.
    let config = vault.join(".norn").join("config.yaml");
    if permission_bits_enforced() {
        std::fs::write(&config, "validate:\n  rules: []\n").unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o000)).unwrap();
        let out = count("chmod-000");
        let stderr = stderr_of(&out);
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            out.status.code(),
            Some(1),
            "an unreadable config must not be served as absent; stderr was: {stderr:?}"
        );
        assert!(
            stderr.contains("norn: failed to read config "),
            "expected the read-failure diagnostic, got: {stderr:?}"
        );
        std::fs::remove_file(&config).unwrap();
    }

    // (2) The config path is a DIRECTORY — `EISDIR`/`ENOTDIR`, which no uid or
    // capability bypasses. The stat succeeds, the read fails.
    std::fs::create_dir_all(&config).unwrap();
    let out = count("as-directory");
    let stderr = stderr_of(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "a config-as-directory must not be served as absent; stderr was: {stderr:?}"
    );
    assert!(
        stderr.contains("norn: failed to read config "),
        "expected the read-failure diagnostic, got: {stderr:?}"
    );

    let _ = std::fs::remove_dir_all(&runtime_dir);
}

/// NRN-475 companion: a registered `[vaults.<name>].config` override pointing at
/// a MISSING file is an owner error (exit 1) — the owner does not fall back to
/// the vault's default path — so it must not collide with the no-config identity
/// of a warm owner summoned before the override was set.
#[cfg(unix)]
#[test]
fn a_registered_override_pointing_at_nothing_is_not_served_as_absent() {
    let guard = tempfile::tempdir().unwrap();
    let vault = guard.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::write(vault.join("a.md"), "---\ntype: note\ntitle: A\n---\nbody\n").unwrap();

    let runtime_dir = isolated_runtime_dir("missingovr");
    let cfg_home = tempfile::tempdir().unwrap();

    let out = norn_summoning(&runtime_dir, cfg_home.path())
        .args(["vault", "register", "reg"])
        .arg(&vault)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "register: {:?}",
        stderr_of(&out)
    );

    // Warm an owner while the vault has no config at all.
    let warm = norn_summoning(&runtime_dir, cfg_home.path())
        .args(["--vault", "reg", "count"])
        .output()
        .unwrap();
    assert_eq!(
        warm.status.code(),
        Some(0),
        "warming must succeed; stderr was: {:?}",
        stderr_of(&warm)
    );

    // Point the registration at a config that does not exist.
    let missing = guard.path().join("gone.yaml");
    let out = norn_summoning(&runtime_dir, cfg_home.path())
        .args(["vault", "set", "reg", "--config"])
        .arg(&missing)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "set: {:?}", stderr_of(&out));

    let out = norn_summoning(&runtime_dir, cfg_home.path())
        .args(["--vault", "reg", "count"])
        .output()
        .unwrap();

    let _ = std::fs::remove_dir_all(&runtime_dir);

    let stderr = stderr_of(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "a missing override must not be served by the warm no-config owner; stderr was: {stderr:?}"
    );
    assert!(
        stderr.contains("norn: failed to read config "),
        "expected the read-failure diagnostic, got: {stderr:?}"
    );
}

/// NRN-475: `Path::exists()` answers `false` for ANY stat failure, not just
/// absence — including `EACCES` on the config's PARENT directory — so the owner
/// took its run-under-defaults branch for a config it was merely forbidden to
/// see, and served the vault unvalidated at exit 0. Only `NotFound` means
/// defaults now; every other stat error is the same hard error the read path
/// raises.
///
/// Repro shape: warm an owner on a config-less vault, then `chmod 000` the
/// `.norn/` directory around a perfectly readable config file.
#[cfg(unix)]
#[test]
fn a_config_hidden_by_an_unreadable_parent_dir_is_not_served_as_absent() {
    use std::os::unix::fs::PermissionsExt;

    // The whole premise is a traversal denial, which a DAC-bypassing process
    // does not experience.
    if !permission_bits_enforced() {
        return;
    }

    let guard = tempfile::tempdir().unwrap();
    let vault = guard.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::write(
        vault.join("a.md"),
        "---\ntype: note\ntitle: A\nstatus: backlog\n---\nbody\n",
    )
    .unwrap();

    let runtime_dir = isolated_runtime_dir("parentdir");
    let cfg_home = tempfile::tempdir().unwrap();

    // Warm an owner with no config present: served under defaults, exit 0.
    let warm = norn_summoning(&runtime_dir, cfg_home.path())
        .arg("-C")
        .arg(&vault)
        .args(["count"])
        .output()
        .unwrap();
    assert_eq!(
        warm.status.code(),
        Some(0),
        "warming must succeed; stderr was: {:?}",
        stderr_of(&warm)
    );

    // A readable config file inside a directory that denies traversal.
    let norn_dir = vault.join(".norn");
    std::fs::create_dir_all(&norn_dir).unwrap();
    std::fs::write(
        norn_dir.join("config.yaml"),
        "validate:\n  rules:\n    - name: notes\n      allowed_values:\n        status:\n          - backlog\n          - done\n",
    )
    .unwrap();
    std::fs::set_permissions(&norn_dir, std::fs::Permissions::from_mode(0o000)).unwrap();

    let out = norn_summoning(&runtime_dir, cfg_home.path())
        .arg("-C")
        .arg(&vault)
        .args(["set", "a.md", "--field", "status=bogus", "--yes"])
        .output()
        .unwrap();

    // Restore before any assertion so the tempdir always cleans up.
    std::fs::set_permissions(&norn_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _ = std::fs::remove_dir_all(&runtime_dir);

    let stderr = stderr_of(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "a config behind an unreadable parent must not be served as absent; stderr was: {stderr:?}"
    );
    assert!(
        stderr.contains("norn: failed to read config "),
        "expected the read-failure diagnostic, got: {stderr:?}"
    );
    assert_eq!(
        std::fs::read_to_string(vault.join("a.md")).unwrap(),
        "---\ntype: note\ntitle: A\nstatus: backlog\n---\nbody\n",
        "the write must not have landed"
    );
}

/// NRN-475: the vault registry supplies the schema override, so a registry the
/// CLI cannot read must refuse rather than degrade to "unregistered". Degrading
/// dropped the override and ran the vault under its (absent) default config, so
/// a write the registered schema forbids landed at exit 0 — but only through
/// `-C` and `NORN_ROOT`, since `--vault` and the directory binding resolve
/// THROUGH the registry and were already failing loud. All four vias now refuse
/// identically.
#[cfg(unix)]
#[test]
fn an_unreadable_registry_refuses_on_every_addressing_via() {
    use std::os::unix::fs::PermissionsExt;

    // The registry is made unreadable with permission bits, which a
    // DAC-bypassing process ignores.
    if !permission_bits_enforced() {
        return;
    }

    let guard = tempfile::tempdir().unwrap();
    let vault = guard.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    let seeded = "---\ntype: note\ntitle: A\nstatus: backlog\n---\nbody\n";
    std::fs::write(vault.join("a.md"), seeded).unwrap();
    // The schema lives outside the vault, reachable only via the registration.
    let override_config = guard.path().join("schema.yaml");
    std::fs::write(
        &override_config,
        "validate:\n  rules:\n    - name: notes\n      allowed_values:\n        status:\n          - backlog\n          - done\n",
    )
    .unwrap();

    let runtime_dir = isolated_runtime_dir("badregistry");
    let cfg_home = tempfile::tempdir().unwrap();

    let out = norn_summoning(&runtime_dir, cfg_home.path())
        .args(["vault", "register", "reg"])
        .arg(&vault)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "register: {:?}",
        stderr_of(&out)
    );
    let out = norn_summoning(&runtime_dir, cfg_home.path())
        .args(["vault", "set", "reg", "--config"])
        .arg(&override_config)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "set: {:?}", stderr_of(&out));

    // Make the registry unreadable.
    let registry_file = cfg_home.path().join("config.toml");
    std::fs::set_permissions(&registry_file, std::fs::Permissions::from_mode(0o000)).unwrap();

    let forbidden = ["set", "a.md", "--field", "status=bogus", "--yes"];
    let mut outcomes: Vec<(&str, Option<i32>, String)> = Vec::new();
    let mut record = |label: &'static str, cmd: &mut Command| {
        let out = cmd.output().unwrap();
        outcomes.push((label, out.status.code(), stderr_of(&out)));
    };
    record(
        "--vault",
        norn_summoning(&runtime_dir, cfg_home.path())
            .args(["--vault", "reg"])
            .args(forbidden),
    );
    record(
        "-C",
        norn_summoning(&runtime_dir, cfg_home.path())
            .arg("-C")
            .arg(&vault)
            .args(forbidden),
    );
    record(
        "NORN_ROOT",
        norn_summoning(&runtime_dir, cfg_home.path())
            .env("NORN_ROOT", &vault)
            .args(forbidden),
    );
    record(
        "cwd",
        norn_summoning(&runtime_dir, cfg_home.path())
            .current_dir(&vault)
            .args(forbidden),
    );

    // Restore before asserting so the tempdir always cleans up.
    std::fs::set_permissions(&registry_file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let _ = std::fs::remove_dir_all(&runtime_dir);

    for (label, code, stderr) in &outcomes {
        assert_eq!(
            *code,
            Some(1),
            "via {label}: an unreadable registry must refuse, not fall back; stderr was: {stderr:?}"
        );
        // Text convergence, not just exit-code convergence. `--vault` and the
        // cwd binding fail earlier, inside registry resolution, than `-C` and
        // `NORN_ROOT` do — every one of them must still render the same
        // headline and the same recovery hint, and the hint must point at the
        // registry file rather than at the per-vault YAML.
        assert!(
            stderr.contains("norn: failed to read config "),
            "via {label}: expected the shared registry-read headline, got: {stderr:?}"
        );
        assert!(
            stderr.contains("hint: repair or remove the registry file by hand"),
            "via {label}: expected the registry-recovery hint, got: {stderr:?}"
        );
        assert!(
            !stderr.contains("YAML") && !stderr.contains("norn config validate"),
            "via {label}: the registry is TOML; the per-vault-YAML advice is wrong here: {stderr:?}"
        );
    }
    // One underlying error, one rendering: the four vias must not differ by a
    // single character of stderr.
    let rendered: std::collections::BTreeSet<&str> =
        outcomes.iter().map(|(_, _, s)| s.as_str()).collect();
    assert_eq!(
        rendered.len(),
        1,
        "every via must render the same diagnostic, got: {rendered:?}"
    );
    assert_eq!(
        std::fs::read_to_string(vault.join("a.md")).unwrap(),
        seeded,
        "no via may have applied the forbidden value"
    );
}

/// NRN-470: `describe --schema --format json` is the surface carrying the whole
/// declared config, and the default `--format json` is the counts projection of
/// what bare `describe` prints. Driven end-to-end through a summon against a
/// vault with a real `.norn/config.yaml`, so the payload split is pinned where
/// a consumer actually meets it.
#[cfg(unix)]
#[test]
fn describe_schema_json_carries_the_declared_config_and_default_json_counts_it() {
    use std::time::{SystemTime, UNIX_EPOCH};

    // A NON-hidden temp root: the cache scan skips a vault whose path carries a
    // dot-prefixed component, and `tempdir()`'s default prefix is `.tmp`.
    let vault = tempfile::Builder::new()
        .prefix("nrn470-vault")
        .tempdir()
        .unwrap();
    let norn_dir = vault.path().join(".norn");
    std::fs::create_dir_all(&norn_dir).unwrap();
    std::fs::write(
        norn_dir.join("config.yaml"),
        r#"inbox:
  path: Inbox

validate:
  required_frontmatter:
    - title
  rules:
    - name: note-rule
      match:
        path: "notes/**/*.md"
      required_frontmatter:
        - type
      frontmatter_defaults:
        type: note
    - name: task
      target: "tasks/{{var.slug}}.md"
      frontmatter_defaults:
        type: task
"#,
    )
    .unwrap();
    std::fs::create_dir_all(vault.path().join("notes")).unwrap();
    std::fs::write(
        vault.path().join("notes/a.md"),
        "---\ntitle: A\ntype: note\n---\nbody\n",
    )
    .unwrap();

    // Short, unique runtime dir for the summoned owner's socket (`sun_path`
    // limit), plus an isolated central-config home.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let runtime_dir = std::path::PathBuf::from(format!("/tmp/nrn470-{}", nanos % 100_000_000));
    let _ = std::fs::remove_dir_all(&runtime_dir);
    let cfg_home = tempfile::tempdir().unwrap();

    let describe = |extra: &[&str]| {
        let mut cmd = norn();
        cmd.arg("-C").arg(vault.path()).arg("describe");
        cmd.args(extra);
        cmd.args(["--format", "json"])
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("NORN_CONFIG_DIR", cfg_home.path())
            .output()
            .unwrap()
    };

    let dump = describe(&["--schema"]);
    let summary = describe(&[]);
    let _ = std::fs::remove_dir_all(&runtime_dir); // best-effort cleanup

    // `--schema`: the declared config in full.
    assert_eq!(
        dump.status.code(),
        Some(0),
        "stderr was: {:?}",
        stderr_of(&dump)
    );
    let v: serde_json::Value = serde_json::from_str(&stdout_of(&dump)).unwrap();
    assert_eq!(v["folders"], serde_json::json!(["notes"]));
    assert_eq!(v["path_rules"][0]["glob"], "notes/**/*.md");
    assert_eq!(v["path_rules"][0]["name"], "note-rule");
    assert_eq!(v["path_rules"][0]["frontmatter_defaults"]["type"], "note");
    assert_eq!(v["creatable_rules"][0]["name"], "task");
    assert_eq!(v["creatable_rules"][0]["target"], "tasks/{{var.slug}}.md");
    assert_eq!(
        v["creatable_rules"][0]["required_vars"],
        serde_json::json!(["slug"])
    );
    assert_eq!(v["inbox"], "Inbox");
    assert_eq!(
        v["schema"]["required_frontmatter"],
        serde_json::json!(["title"])
    );
    assert_eq!(v["schema"]["rules"][0]["name"], "note-rule");
    assert_eq!(v["schema"]["rules"].as_array().unwrap().len(), 2);

    // Bare: the counts projection — four scalar keys, no declared config.
    assert_eq!(
        summary.status.code(),
        Some(0),
        "stderr was: {:?}",
        stderr_of(&summary)
    );
    assert_eq!(
        stdout_of(&summary).trim_end(),
        r#"{"folders":1,"path_rules":1,"creatable_rules":1,"inbox":"Inbox"}"#
    );
}
