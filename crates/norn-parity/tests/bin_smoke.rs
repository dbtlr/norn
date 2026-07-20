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
    // NRN-383 adds two `mcp` suite cases (initialize/tools/list handshake +
    // a tools/call), both `ported: false` — self-check ignores `ported` and
    // runs every case, so the total grew from 56 to 58. NRN-378 adds seven
    // `mutate` cases (set/new forecast + refusal), taking the total to 65.
    // NRN-388 adds eight more `mutate` cases (confirmed applies + report bodies:
    // two records applies, a push --format json apply, a warning-bearing json
    // forecast, two refusal-body cases, and the two NRN-371 null-/comment-block
    // promotions), taking the total to 73; all must Match (oracle vs. itself —
    // the confirmed-apply cases via the per-case trace-id normalization).
    // NRN-379 adds five `edit` cases (a confirmed apply, a dry-run forecast, and
    // three refusal/json-ops shapes), taking the total to 78; all Match.
    assert!(
        stdout.contains("78 cases: 78 match, 0 diverged, 0 drift, 0 stale entries"),
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
    // case, and six describe cases; NRN-381 adds seven validate cases (summary
    // json/records, paths, code/severity-filtered json/jsonl) — all 42
    // find/count/get/describe/validate cases must Match the oracle (pure
    // byte-parity, no ledger entry). Fourteen cases diverge
    // with ledger entries: the three help cases (help-bare by the `vault`
    // namespace + GLOBAL OPTIONS PD-101, help-find / help-validate by the GLOBAL
    // OPTIONS change PD-102), the two text-layer edge cases (NRN-350 code-opacity
    // PD-103, NRN-349 BOM PD-104), the two CLI-semantics slate cases (zero-indexed
    // `--starts-at` PD-105, last-wins `--limit`/`--no-limit` PD-106), the two
    // URL-semantics slate cases (Markdown-link split-then-decode + block-ref
    // PD-107, external-vs-local scheme classification PD-108), and the five
    // presentation/errors slate cases (the soft-landing diagnostic surface — now
    // four shapes under PD-109, including the NRN-367 owner-side dynamic-field
    // gate's unknown-field rejection — and grammar-wide last-wins PD-110) —
    // covered divergences, not drift.
    // NRN-378 adds seven ported `mutate` cases (set/new forecast + refusal),
    // every one a byte-exact match against the oracle (no ledger entry): the
    // gated total grows from 56 to 63 and the match count from 42 to 49.
    // NRN-388 adds eight more ported `mutate` cases — five MATCH (two records
    // applies, a push --format json apply, a value-not-allowed and a
    // field-conflict refusal body) and three DIVERGE with new ledger entries
    // (the unified --format json warning envelope PD-111, and the two NRN-371
    // null-/comment-only frontmatter mapping-promotions PD-112) — so the gated
    // total grows to 71, the match count to 54, and the diverged count to 17.
    // NRN-379 adds five ported `edit` cases, every one a byte-exact match (no
    // ledger entry): the gated total grows to 76 and the match count to 59; the
    // diverged count stays 17.
    assert!(
        stdout.contains("76 cases: 59 match, 17 diverged, 0 drift, 0 stale entries"),
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
        "PD-109",
        "PD-110",
        "PD-111",
        "PD-112",
        "mutate-set-apply-records-zoo",
        "mutate-new-unknown-field-warning-json-zoo",
        "mutate-set-null-block-promote",
        "mutate-set-comment-block-promote",
        "text-edge-bom-doc-all-cols",
        "text-edge-code-fenced-block-id-link",
        "url-edge-decode-split-blockref",
        "url-edge-scheme-classification",
        "read-find-starts-at-zero-indexed-zoo",
        "read-find-limit-nolimit-last-wins-zoo",
        "err-malformed-config",
        "err-repeated-limit-last-wins-zoo",
        "help-find",
        "help-validate",
        "read-find-json-zoo",
        "read-count-clean",
        "validate-summary-zoo",
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
