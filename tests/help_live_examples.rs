//! Integration tests for the Phase 3 LIVE EXAMPLES block on `vault find --help`.
//!
//! Drives the real `vault` binary against on-disk fixture vaults so the full
//! interceptor path (arg parse → cwd resolve → cache open → generator →
//! renderer) is exercised. The existing test convention uses
//! `env!("CARGO_BIN_EXE_norn")` to locate the binary; this file follows
//! the same pattern.

use std::process::Command;

use camino::Utf8PathBuf;
use tempfile::TempDir;

/// Pre-write a FRESH lazy-sweep throttle marker (`<cache_home>/norn/.last-prune`)
/// so norn invocations under this cache home never spawn a detached GC sweep
/// child (NRN-287) that could race this test. Mirrors src/cache/prune.rs
/// `PRUNE_MARKER`.
fn prewrite_prune_marker(cache_home: &std::path::Path) {
    let tree = cache_home.join("norn");
    let _ = std::fs::create_dir_all(&tree);
    let _ = std::fs::write(tree.join(".last-prune"), b"");
}

/// Build a `norn` Command with `XDG_CACHE_HOME`/`XDG_STATE_HOME` isolated to
/// hidden subdirs of the fixture tempdir (hidden so the vault walker skips
/// them), so the binary never reads or sweeps the developer's real
/// cache/state trees. Keying isolation off the fixture root means the
/// `cache rebuild` in `fixture_vault` and the test's own invocation share
/// one isolated cache — required because the live-examples generator only
/// opens the cache, never rebuilds it.
fn norn_bin(fixture_root: &std::path::Path) -> Command {
    prewrite_prune_marker(&fixture_root.join(".xdg-cache"));
    let mut c = Command::new(env!("CARGO_BIN_EXE_norn"));
    c.env("XDG_CACHE_HOME", fixture_root.join(".xdg-cache"))
        .env("XDG_STATE_HOME", fixture_root.join(".xdg-state"));
    c
}

/// Build a small fixture vault on disk: empty `.norn/` so it's recognized
/// as a vault root, plus on-disk Markdown files whose top-level frontmatter
/// matches the Phase 3 algorithm. Pre-populates the cache by running
/// `vault cache rebuild` so the live-examples generator (which only opens
/// the cache, never rebuilds it) sees populated rows.
fn fixture_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-help-live-integ-")
        .tempdir()
        .unwrap();
    let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
    std::fs::create_dir_all(root.join(".norn").as_std_path()).unwrap();
    let docs: &[(&str, &str)] = &[
        (
            "a.md",
            "---\ntype: note\nworkspace: norn\nmodified: 2026-05-21\n---\n",
        ),
        (
            "b.md",
            "---\ntype: note\nworkspace: norn\nmodified: 2026-05-20\n---\n",
        ),
        (
            "c.md",
            "---\ntype: note\nworkspace: norn\nmodified: 2026-05-19\n---\n",
        ),
        (
            "d.md",
            "---\ntype: task\nworkspace: norn\nmodified: 2026-05-18\n---\n",
        ),
        (
            "e.md",
            "---\ntype: task\nworkspace: atlas\nmodified: 2026-05-17\n---\n",
        ),
    ];
    for (path, body) in docs {
        std::fs::write(root.join(path).as_std_path(), body).unwrap();
    }
    let out = norn_bin(tmp.path())
        .args(["--cwd", tmp.path().to_str().unwrap(), "cache", "rebuild"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "cache rebuild failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    tmp
}

#[test]
fn long_help_inside_vault_emits_live_examples_block() {
    let vault = fixture_vault();
    let out = norn_bin(vault.path())
        .env("NO_COLOR", "1")
        .env("PAGER", "cat")
        .args(["--cwd", vault.path().to_str().unwrap(), "find", "--help"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("LIVE EXAMPLES"),
        "expected LIVE EXAMPLES; got:\n{stdout}"
    );
    // The find_live composer ranks enum-like fields by
    // `(docs_with_field/total) * (top_value/docs_with_field)` =
    // `top_value_doc_count / total_documents`. For this fixture:
    //   workspace: top_value=4 ("norn") → 4/5 = 0.8
    //   type:      top_value=3 ("note")      → 3/5 = 0.6
    // So workspace wins P1, type wins P2.
    assert!(
        stdout.contains("norn find --eq workspace:norn --eq type:note --sort modified --limit 5"),
        "expected composed query; got:\n{stdout}"
    );
    assert!(
        stdout.contains("3 documents match"),
        "expected match count; got:\n{stdout}"
    );
}

#[test]
fn long_help_deterministic_across_runs() {
    let vault = fixture_vault();
    let run = || {
        let out = norn_bin(vault.path())
            .env("NO_COLOR", "1")
            .env("PAGER", "cat")
            .args(["--cwd", vault.path().to_str().unwrap(), "find", "--help"])
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    };
    let a = run();
    let b = run();
    assert_eq!(
        a, b,
        "consecutive --help invocations must produce identical output"
    );
}

#[test]
fn long_help_outside_vault_has_no_live_examples() {
    let tmp = tempfile::Builder::new()
        .prefix("norn-help-no-vault-")
        .tempdir()
        .unwrap();
    let out = norn_bin(tmp.path())
        .env("NO_COLOR", "1")
        .env("PAGER", "cat")
        .args(["--cwd", tmp.path().to_str().unwrap(), "find", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        !stdout.contains("LIVE EXAMPLES"),
        "no-vault path must omit LIVE EXAMPLES; got:\n{stdout}"
    );
}

#[test]
fn short_help_never_emits_live_examples() {
    let vault = fixture_vault();
    let out = norn_bin(vault.path())
        .env("NO_COLOR", "1")
        .args(["--cwd", vault.path().to_str().unwrap(), "find", "-h"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        !stdout.contains("LIVE EXAMPLES"),
        "short form must omit LIVE EXAMPLES; got:\n{stdout}"
    );
}

#[test]
fn long_help_ascii_marker_under_norn_ascii() {
    let vault = fixture_vault();
    let out = norn_bin(vault.path())
        .env("NO_COLOR", "1")
        .env("NORN_ASCII", "1")
        .env("PAGER", "cat")
        .args(["--cwd", vault.path().to_str().unwrap(), "find", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("> norn find"),
        "expected '> norn find' under NORN_ASCII; got:\n{stdout}"
    );
    assert!(
        !stdout.contains("▸ norn find"),
        "must not emit UTF marker under NORN_ASCII; got:\n{stdout}"
    );
}

#[test]
fn long_help_no_color_includes_live_tag() {
    let vault = fixture_vault();
    let out = norn_bin(vault.path())
        .env("NO_COLOR", "1")
        .env("PAGER", "cat")
        .args(["--cwd", vault.path().to_str().unwrap(), "find", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("3 documents match (live)"),
        "expected '(live)' suffix under NO_COLOR; got:\n{stdout}"
    );
}
