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
    // Self-check runs oracle vs. itself over EVERY declared case (`ported` is
    // ignored), so all match and none diverge. The count is DERIVED from the
    // case catalog rather than hand-pinned per PR — a new case updates the
    // expected summary automatically (NRN-421 harness-fitness). This summary
    // string is one of the two deliberate suite-behavior pins the harness keeps
    // (the other is the gated summary below): they assert the runner's own
    // tally, so they are computed here rather than read back from the ledger.
    let total = norn_parity::cases::all_case_ids().len();
    let expected = format!("{total} cases: {total} match, 0 diverged, 0 drift, 0 stale entries");
    assert!(
        stdout.contains(&expected),
        "expected the derived all-match summary `{expected}`, got:\n{stdout}"
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
    // The gated summary and the per-row needles are DERIVED from the case
    // catalog and the divergence ledger, not hand-kept per PR (NRN-421
    // harness-fitness): a new case or ledger entry updates these assertions
    // automatically. The ported/diverged/match tally is computed from the same
    // two sources the runner itself gates on; every gated (ported) case appears
    // as a report row; every ledger entry id appears in the ENTRY column of the
    // row(s) it gates. This gated summary is one of the two deliberate
    // suite-behavior hand-pins (see the self-check summary above) — it asserts
    // the runner's own tally, computed here, not read back verbatim.
    let ledger_path = workspace.join("docs/parity-ledger.toml");
    let all_ids: std::collections::BTreeSet<&str> =
        norn_parity::cases::all_case_ids().into_iter().collect();
    let ported: Vec<&str> = norn_parity::cases::ported_case_ids();
    let ported_set: std::collections::BTreeSet<&str> = ported.iter().copied().collect();
    let ledger = norn_parity::Ledger::load(&ledger_path, &all_ids, &ported_set)
        .unwrap_or_else(|e| panic!("load parity ledger: {e}"));

    // A gated case diverges iff the ledger cites it; the loader guarantees every
    // cited case is ported, so the cited set IS the diverged set.
    let diverged: std::collections::BTreeSet<&str> = ledger
        .entries
        .iter()
        .flat_map(|e| e.cases.iter().map(String::as_str))
        .collect();
    let gated_total = ported.len();
    let diverged_count = diverged.len();
    let match_count = gated_total - diverged_count;
    let expected = format!(
        "{gated_total} cases: {match_count} match, {diverged_count} diverged, 0 drift, 0 stale entries"
    );
    assert!(
        stdout.contains(&expected),
        "expected the derived gated summary `{expected}`, got:\n{stdout}"
    );

    // Every ported case is reported as a row (match or diverged).
    for case_id in &ported {
        assert!(
            stdout.contains(case_id),
            "expected the gated report to list ported case `{case_id}`, got:\n{stdout}"
        );
    }
    // Every ledger entry gates at least one ran+diverged case, so its id must
    // appear in the report's ENTRY column.
    for entry in &ledger.entries {
        assert!(
            stdout.contains(entry.id.as_str()),
            "expected the gated report to cite ledger entry `{}`, got:\n{stdout}",
            entry.id
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
