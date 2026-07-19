//! End-to-end smoke tests against the built `norn-parity` binary
//! (`env!("CARGO_BIN_EXE_norn-parity")`), matching the CI recipe
//! (`.github/workflows/ci.yml`'s "parity self-check + consistency" step).
//!
//! Every invocation sets an explicit `current_dir` and passes `--oracle`
//! / `--rewrite` explicitly rather than relying on the bin's own
//! PATH/relative-path defaults — the test binary's own cwd under `cargo
//! test` is not a documented contract, so nothing here leaves resolution
//! to chance.

mod common;

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_norn-parity")
}

#[test]
fn self_check_end_to_end_is_all_match_exit_0() {
    if common::oracle_missing("bin_smoke") {
        return;
    }
    let workspace = common::workspace_root();

    // Deliberately points --rewrite at a nonexistent path: self-check runs
    // oracle-vs-oracle and must not require the rewrite artifact (its whole
    // purpose is vetting a case set before any rewrite binary exists).
    let output = Command::new(bin())
        .current_dir(&workspace)
        .arg("--self-check")
        .args(["--oracle", "norn"])
        .args(["--rewrite", "/nonexistent/rewrite-norn"])
        .output()
        .expect("failed to run norn-parity --self-check");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected exit 0, got {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    );
    assert!(
        stdout.contains("11 cases: 11 match, 0 diverged, 0 drift, 0 stale entries"),
        "expected the exact all-match summary, got:\n{stdout}"
    );
    assert!(
        !stdout
            .lines()
            .any(|l| l.contains("  drift  ") || l.trim_end().ends_with("drift")),
        "expected no per-case drift rows, got:\n{stdout}"
    );
}

#[test]
fn default_mode_gates_help_cases_exit_0() {
    if common::oracle_missing("bin_smoke") {
        return;
    }
    let workspace = common::workspace_root();
    let rewrite = common::rewrite_debug_binary();

    let output = Command::new(bin())
        .current_dir(&workspace)
        .args(["--oracle", "norn"])
        .arg("--rewrite")
        .arg(&rewrite)
        .output()
        .expect("failed to run norn-parity (default mode)");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected exit 0 (phase-1: help cases match or diverge-with-entry, zero drift), got {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    );
    // NRN-345 reshapes the GLOBAL OPTIONS block on every command (drops
    // `--config`, adds `--vault`), so all three gated help cases now diverge:
    // help-bare by the `vault` namespace + GLOBAL OPTIONS (PD-101), and
    // help-find / help-validate by the GLOBAL OPTIONS change (PD-102) — every
    // one a covered divergence, not drift.
    assert!(
        stdout.contains("3 cases: 0 match, 3 diverged, 0 drift, 0 stale entries"),
        "expected the exact gated summary, got:\n{stdout}"
    );
    for needle in [
        "help-bare",
        "diverged",
        "PD-101",
        "PD-102",
        "help-find",
        "help-validate",
    ] {
        assert!(
            stdout.contains(needle),
            "expected the gated report to mention `{needle}`, got:\n{stdout}"
        );
    }
    assert!(
        !stdout
            .lines()
            .any(|l| l.contains("  drift  ") || l.trim_end().ends_with("drift")),
        "expected no per-case drift rows, got:\n{stdout}"
    );
}

#[test]
fn consistency_mode_exits_0_with_no_disagreements() {
    if common::oracle_missing("bin_smoke") {
        return;
    }
    let workspace = common::workspace_root();

    let output = Command::new(bin())
        .current_dir(&workspace)
        .args(["--consistency", "--oracle", "norn"])
        .output()
        .expect("failed to run norn-parity --consistency");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected exit 0, got {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    );
}
