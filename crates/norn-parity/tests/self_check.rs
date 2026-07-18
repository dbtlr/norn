//! Self-check end-to-end: the library's oracle-vs-itself path over every
//! declared case must be all-Match. This is what proves the starter case
//! set and the comparator are sound against the real oracle, independent
//! of the (still phase-0, unported) rewrite binary.

mod common;

use norn_parity::run::{self, Mode, RunConfig};
use norn_parity::Verdict;

#[test]
fn every_case_matches_oracle_vs_itself() {
    if common::oracle_missing("self_check") {
        return;
    }

    let oracle = common::oracle_path();
    let rewrite = common::rewrite_debug_binary();
    let ledger_path = common::workspace_root().join("docs/parity-ledger.toml");

    let config = RunConfig {
        mode: Mode::SelfCheck,
        oracle: &oracle,
        rewrite: &rewrite,
        ledger_path: &ledger_path,
        suite_filter: &[],
    };

    let report = run::run(&config).unwrap_or_else(|e| panic!("self-check run failed: {e}"));

    assert_eq!(
        report.oracle_version, "0.48.0",
        "expected the pinned oracle version"
    );
    assert!(
        !report.outcomes.is_empty(),
        "self-check ignores `ported` and should run every declared case"
    );
    assert_eq!(
        report.outcomes.len(),
        norn_parity::cases::all_case_ids().len(),
        "self-check should cover every case across every suite"
    );

    let non_matching: Vec<&str> = report
        .outcomes
        .iter()
        .filter(|o| !matches!(o.verdict, Verdict::Match))
        .map(|o| o.case_id)
        .collect();
    assert!(
        non_matching.is_empty(),
        "self-check must be all-Match (ledger is not consulted); non-matching cases: {non_matching:?}"
    );
    assert!(
        report.stale_entries.is_empty(),
        "self-check has stale checks off"
    );
    assert_eq!(report.exit_code(), 0);
}
