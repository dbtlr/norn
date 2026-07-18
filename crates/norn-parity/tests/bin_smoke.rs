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
    if !common::oracle_present() {
        eprintln!("skip: `norn` not found on PATH — bin_smoke skipped");
        return;
    }
    let workspace = common::workspace_root();
    let rewrite = common::rewrite_debug_binary();

    let output = Command::new(bin())
        .current_dir(&workspace)
        .arg("--self-check")
        .args(["--oracle", "norn"])
        .arg("--rewrite")
        .arg(&rewrite)
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
        stdout.contains("0 drift"),
        "expected an all-match summary, got:\n{stdout}"
    );
    assert!(
        !stdout
            .lines()
            .any(|l| l.contains("  drift  ") || l.trim_end().ends_with("drift")),
        "expected no per-case drift rows, got:\n{stdout}"
    );
}

#[test]
fn default_mode_reports_zero_suites_gated_exit_0() {
    if !common::oracle_present() {
        eprintln!("skip: `norn` not found on PATH — bin_smoke skipped");
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
        "expected exit 0 (phase-0: zero ported suites), got {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    );
    assert_eq!(stdout.trim(), "0 suites gated");
}

#[test]
fn consistency_mode_exits_0_with_no_disagreements() {
    if !common::oracle_present() {
        eprintln!("skip: `norn` not found on PATH — bin_smoke skipped");
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
