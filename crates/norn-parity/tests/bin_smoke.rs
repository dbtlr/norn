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
        stdout.contains("47 cases: 47 match, 0 diverged, 0 drift, 0 stale entries"),
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
        "expected exit 0 (find/count match, help cases diverge-with-entry, zero drift), got {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    );
    // NRN-346 ports find + count; NRN-347 adds three deep-facet find cases, nine
    // get cases (incl. --section + alias addressing), a records-format deep-facet
    // case, and six describe cases — all 35 find/count/get/describe cases must
    // Match the oracle (pure byte-parity, no ledger entry). Nine cases diverge
    // with ledger entries: the three help cases (help-bare by the `vault`
    // namespace + GLOBAL OPTIONS PD-101, help-find / help-validate by the GLOBAL
    // OPTIONS change PD-102), the two text-layer edge cases (NRN-350 code-opacity
    // PD-103, NRN-349 BOM PD-104), the two CLI-semantics slate cases
    // (zero-indexed `--starts-at` PD-105, last-wins `--limit`/`--no-limit`
    // PD-106), and the two URL-semantics slate cases (Markdown-link
    // split-then-decode + block-ref PD-107, external-vs-local scheme
    // classification PD-108) — covered divergences, not drift.
    assert!(
        stdout.contains("44 cases: 35 match, 9 diverged, 0 drift, 0 stale entries"),
        "expected the exact gated summary, got:\n{stdout}"
    );
    for needle in [
        "help-bare",
        "diverged",
        "PD-101",
        "PD-102",
        "PD-103",
        "PD-104",
        "PD-105",
        "PD-106",
        "PD-107",
        "PD-108",
        "text-edge-bom-doc-all-cols",
        "text-edge-code-fenced-block-id-link",
        "url-edge-decode-split-blockref",
        "url-edge-scheme-classification",
        "read-find-starts-at-zero-indexed-zoo",
        "read-find-limit-nolimit-last-wins-zoo",
        "help-find",
        "help-validate",
        "read-find-json-zoo",
        "read-count-clean",
        "match",
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
