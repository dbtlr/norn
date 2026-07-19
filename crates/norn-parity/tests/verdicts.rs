//! End-to-end verdict tests: drive the REAL `run::run_suites` orchestration
//! (the production seam under `run::run`) over a synthetic one-case catalog,
//! forcing each of Drift / Diverged / stale and asserting the exit code.
//!
//! Why a synthetic catalog: the production catalog has zero `ported` cases in
//! phase 0, so neither a gated run nor a ledger entry could ever cite a real
//! case (the ledger rejects entries for unported surfaces). `run_suites`
//! takes an explicit `&'static [Suite]` so a test can inject a single ported
//! case and exercise the classification/stale glue exactly as production
//! would — no re-derivation of that logic inline.
//!
//! The candidate binary is `/bin/echo` (present on macOS + Linux,
//! deterministic, and guaranteed to mismatch the oracle's `--help` output) for
//! the mismatch cases, and the oracle itself for the match/stale case. Every
//! ledger here is a temp file; the real `docs/parity-ledger.toml` is never
//! touched.

mod common;

use std::path::Path;

use norn_parity::cases::{Case, Fixture, Suite};
use norn_parity::run::{self, Mode, RunConfig};
use norn_parity::Verdict;

const CLEAN_1: Fixture = Fixture {
    profile_name: "clean",
    seed: 1,
};

const FAB_CASE_ID: &str = "fab-help-clean";

/// A single ported case running `--help` (exits 0 on the oracle) over the
/// clean fixture — the injected catalog for every test below.
///
/// `--help` is deliberately vault-content-independent: its output does not
/// read the on-disk cache, so an oracle-vs-oracle run Matches deterministically
/// even when these three tests execute concurrently. A vault-reading argv
/// (e.g. `count`) is not safe here — the oracle non-deterministically emits a
/// `cache is corrupted (missing schema_version meta row); rebuilding` stderr
/// line on a fresh vault's first touch when several `norn` processes race the
/// cache build, which would flip the stale test's required Match to a
/// Diverged. (The production self-check runs its cases sequentially in one
/// process and is unaffected.) The mismatch tests below still hold: the
/// oracle's help text never equals `/bin/echo`'s echo of the argv.
static FAB_SUITES: &[Suite] = &[Suite {
    name: "fabricated",
    cases: &[Case {
        id: FAB_CASE_ID,
        argv: &["--help"],
        fixture: CLEAN_1,
        stdin: None,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: &[],
    }],
}];

const ECHO: &str = "/bin/echo";

fn diverged_verdicts(report: &run::RunReport) -> Vec<String> {
    report
        .outcomes
        .iter()
        .filter_map(|o| match &o.verdict {
            Verdict::Diverged { entry_id } => Some(entry_id.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn candidate_echo_with_no_ledger_entry_is_drift_and_exits_1() {
    if common::oracle_missing("verdicts") {
        return;
    }
    let ledger_dir = tempfile::TempDir::new().unwrap();
    let ledger_path = ledger_dir.path().join("ledger.toml");
    common::write_ledger(&ledger_path, "[meta]\noracle_version = \"0.48.1\"\n");

    let config = RunConfig {
        mode: Mode::Gated,
        oracle: Path::new("norn"),
        rewrite: Path::new(ECHO),
        ledger_path: &ledger_path,
        suite_filter: &[],
    };
    let report = run::run_suites(&config, FAB_SUITES).expect("run should succeed");

    assert_eq!(
        report.outcomes.len(),
        1,
        "the one ported case runs in gated mode"
    );
    assert_eq!(report.outcomes[0].verdict, Verdict::Drift);
    assert!(report.stale_entries.is_empty());
    assert_eq!(report.exit_code(), 1);
}

#[test]
fn candidate_echo_covered_by_a_ledger_entry_is_diverged_citing_it_and_exits_0() {
    if common::oracle_missing("verdicts") {
        return;
    }
    let ledger_dir = tempfile::TempDir::new().unwrap();
    let ledger_path = ledger_dir.path().join("ledger.toml");
    common::write_ledger(
        &ledger_path,
        &format!(
            r#"
[meta]
oracle_version = "0.48.1"

[[entry]]
id = "TEST-DIVERGED"
surface = "help (fabricated)"
cases = ["{FAB_CASE_ID}"]
old = "help text"
new = "echo of argv"
reason = "decided-better"
decision = "docs/decisions/0018-greenfield-rewrite-oracle-parity.md"
"#
        ),
    );

    let config = RunConfig {
        mode: Mode::Gated,
        oracle: Path::new("norn"),
        rewrite: Path::new(ECHO),
        ledger_path: &ledger_path,
        suite_filter: &[],
    };
    let report = run::run_suites(&config, FAB_SUITES).expect("run should succeed");

    assert_eq!(
        diverged_verdicts(&report),
        vec!["TEST-DIVERGED".to_string()],
        "the mismatch is covered by exactly one entry, cited by id"
    );
    assert!(
        report.stale_entries.is_empty(),
        "the entry's case diverged, so it is not stale"
    );
    assert_eq!(report.exit_code(), 0);
}

#[test]
fn an_entry_citing_a_matching_case_is_stale_and_exits_1() {
    if common::oracle_missing("verdicts") {
        return;
    }
    let ledger_dir = tempfile::TempDir::new().unwrap();
    let ledger_path = ledger_dir.path().join("ledger.toml");
    common::write_ledger(
        &ledger_path,
        &format!(
            r#"
[meta]
oracle_version = "0.48.1"

[[entry]]
id = "TEST-STALE"
surface = "help (fabricated)"
cases = ["{FAB_CASE_ID}"]
old = "help text"
new = "help text"
reason = "decided-better"
decision = "docs/decisions/0018-greenfield-rewrite-oracle-parity.md"
"#
        ),
    );

    // rewrite := the oracle itself. Per-side vaults (finding 5) mean each
    // binary reads its own freshly generated copy, so the two `--help`
    // runs Match; the entry then cites a case that did not diverge -> stale.
    let config = RunConfig {
        mode: Mode::Gated,
        oracle: Path::new("norn"),
        rewrite: Path::new("norn"),
        ledger_path: &ledger_path,
        suite_filter: &[],
    };
    let report = run::run_suites(&config, FAB_SUITES).expect("run should succeed");

    assert_eq!(
        report.outcomes[0].verdict,
        Verdict::Match,
        "oracle vs. oracle over identical vaults must match"
    );
    assert_eq!(report.stale_entries, vec!["TEST-STALE".to_string()]);
    assert_eq!(
        report.exit_code(),
        1,
        "a stale entry fails the run even though its one case matched"
    );
}
