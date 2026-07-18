//! Fabricated-divergence tests: deterministically force a mismatch (and,
//! separately, a forced match) through the compare/verdict/stale-detection
//! path, independent of whether any real case currently diverges.
//!
//! The fabrication technique (per the implementation spec): run the real
//! oracle with two *different* argv against the same fixture vault (`count`
//! vs `count --format json`) so their normalized outputs are guaranteed to
//! differ, and feed that pair through `verdict::outputs_match` /
//! `Ledger::entry_for_case` exactly as `run::run` would for a real
//! oracle-vs-rewrite case. A real, already-declared case id
//! (`read-count-clean`) stands in for "the case" so the ledger's
//! unknown-case-id validation stays meaningful — only the pair of outputs
//! being compared is fabricated, not the case identity.
//!
//! Every ledger used here is a temp file (`common::write_ledger`); the real
//! `docs/parity-ledger.toml` is never touched.

mod common;

use std::collections::BTreeSet;
use std::path::Path;

use norn_parity::exec;
use norn_parity::ledger::Ledger;
use norn_parity::normalize;
use norn_parity::run::{CaseOutcome, RunReport};
use norn_parity::verdict::{self, Verdict};

const FABRICATED_CASE_ID: &str = "read-count-clean";

fn normalized(oracle: &Path, argv: &[&str], vault: &Path) -> normalize::NormalizedOutput {
    let raw = exec::run_argv(oracle, argv, None, vault).expect("oracle invocation failed");
    normalize::normalize_output(&raw, vault, normalize::DEFAULT)
        .expect("oracle process was not signaled")
}

fn known_ids() -> BTreeSet<&'static str> {
    norn_parity::cases::all_case_ids().into_iter().collect()
}

#[test]
fn mismatch_with_no_ledger_entry_is_drift_and_exits_1() {
    if !common::oracle_present() {
        eprintln!("skip: `norn` not found on PATH — verdicts skipped");
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    let vault = dir.path().join("vault");
    norn_fixtures::generate(&norn_fixtures::Profile::clean(), 1, &vault).unwrap();
    let oracle = common::oracle_path();

    let a = normalized(&oracle, &["count"], &vault);
    let b = normalized(&oracle, &["count", "--format", "json"], &vault);
    assert!(
        !verdict::outputs_match(&a, &b),
        "`count` and `count --format json` must produce different output for this fabrication to be meaningful"
    );

    let empty_ledger =
        Ledger::parse("[meta]\noracle_version = \"0.48.0\"\n", &known_ids()).unwrap();
    let verdict = match empty_ledger.entry_for_case(FABRICATED_CASE_ID) {
        Some(entry) => Verdict::Diverged {
            entry_id: entry.id.clone(),
        },
        None => Verdict::Drift,
    };
    assert_eq!(verdict, Verdict::Drift);

    let ran: BTreeSet<&str> = [FABRICATED_CASE_ID].into_iter().collect();
    let diverged: BTreeSet<&str> = BTreeSet::new(); // Drift is never cited, never counted as diverged
    let stale_entries: Vec<String> = empty_ledger
        .stale_entries(&ran, &diverged)
        .into_iter()
        .map(String::from)
        .collect();

    let report = RunReport {
        outcomes: vec![CaseOutcome {
            case_id: FABRICATED_CASE_ID,
            suite_name: "test",
            verdict,
        }],
        stale_entries,
        oracle_version: "0.48.0".to_string(),
    };
    assert_eq!(report.exit_code(), 1);
}

#[test]
fn mismatch_covered_by_a_ledger_entry_is_diverged_citing_it_and_exits_0() {
    if !common::oracle_present() {
        eprintln!("skip: `norn` not found on PATH — verdicts skipped");
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    let vault = dir.path().join("vault");
    norn_fixtures::generate(&norn_fixtures::Profile::clean(), 1, &vault).unwrap();
    let oracle = common::oracle_path();

    let a = normalized(&oracle, &["count"], &vault);
    let b = normalized(&oracle, &["count", "--format", "json"], &vault);
    assert!(!verdict::outputs_match(&a, &b));

    let ledger_dir = tempfile::TempDir::new().unwrap();
    let ledger_path = ledger_dir.path().join("ledger.toml");
    common::write_ledger(
        &ledger_path,
        &format!(
            r#"
[meta]
oracle_version = "0.48.0"

[[entry]]
id = "TEST-DIVERGED"
surface = "count (fabricated)"
cases = ["{FABRICATED_CASE_ID}"]
old = "count (text)"
new = "count --format json"
reason = "decided-better"
decision = "docs/decisions/0018-greenfield-rewrite-oracle-parity.md"
"#
        ),
    );
    let ledger = Ledger::load(&ledger_path, &known_ids()).unwrap();

    let verdict = match ledger.entry_for_case(FABRICATED_CASE_ID) {
        Some(entry) => Verdict::Diverged {
            entry_id: entry.id.clone(),
        },
        None => Verdict::Drift,
    };
    assert_eq!(
        verdict,
        Verdict::Diverged {
            entry_id: "TEST-DIVERGED".to_string()
        }
    );

    let ran: BTreeSet<&str> = [FABRICATED_CASE_ID].into_iter().collect();
    let diverged: BTreeSet<&str> = [FABRICATED_CASE_ID].into_iter().collect();
    let stale_entries: Vec<String> = ledger
        .stale_entries(&ran, &diverged)
        .into_iter()
        .map(String::from)
        .collect();
    assert!(
        stale_entries.is_empty(),
        "the entry's case diverged, so it is not stale"
    );

    let report = RunReport {
        outcomes: vec![CaseOutcome {
            case_id: FABRICATED_CASE_ID,
            suite_name: "test",
            verdict,
        }],
        stale_entries,
        oracle_version: "0.48.0".to_string(),
    };
    assert_eq!(report.exit_code(), 0);
}

#[test]
fn an_entry_citing_a_matching_case_is_stale_and_exits_1() {
    if !common::oracle_present() {
        eprintln!("skip: `norn` not found on PATH — verdicts skipped");
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    let vault = dir.path().join("vault");
    norn_fixtures::generate(&norn_fixtures::Profile::clean(), 1, &vault).unwrap();
    let oracle = common::oracle_path();

    // A genuine, unforced match: `b` is `a`, not a second live invocation.
    //
    // This deliberately does NOT re-run the oracle a second time and rely
    // on the two runs agreeing. Empirically discovered against the real
    // oracle (v0.48.0): a freshly generated vault's on-disk cache
    // sometimes fails to persist its `schema_version` meta row, and once
    // that happens every subsequent invocation against that vault prints a
    // `cache is corrupted (...); rebuilding` line to stderr — but *which*
    // invocation first sees it (the 1st, the 2nd, later, or never) is not
    // consistently reproducible across otherwise-identical fixture
    // regenerations. That is a real oracle finding (reported separately),
    // but it is orthogonal to what this test checks: verdict/staleness
    // composition given a Match precondition never even consults the
    // ledger for the verdict itself (see `run.rs`), so one real
    // invocation, compared to itself, exercises exactly that path without
    // being at the mercy of the oracle's cache-rebuild timing. Real
    // same-process determinism across the whole case set is what
    // `self_check.rs` proves, not this test.
    let a = normalized(&oracle, &["count"], &vault);
    let b = a.clone();
    assert!(verdict::outputs_match(&a, &b));

    let ledger_dir = tempfile::TempDir::new().unwrap();
    let ledger_path = ledger_dir.path().join("ledger.toml");
    common::write_ledger(
        &ledger_path,
        &format!(
            r#"
[meta]
oracle_version = "0.48.0"

[[entry]]
id = "TEST-STALE"
surface = "count (fabricated)"
cases = ["{FABRICATED_CASE_ID}"]
old = "count (text)"
new = "count (text)"
reason = "decided-better"
decision = "docs/decisions/0018-greenfield-rewrite-oracle-parity.md"
"#
        ),
    );
    let ledger = Ledger::load(&ledger_path, &known_ids()).unwrap();

    // Identical argv means Match — the entry is never even consulted for
    // the verdict, exactly as `run::run` would behave.
    let verdict = Verdict::Match;

    let ran: BTreeSet<&str> = [FABRICATED_CASE_ID].into_iter().collect();
    let diverged: BTreeSet<&str> = BTreeSet::new(); // nothing diverged: the cited case matched
    let stale_entries: Vec<String> = ledger
        .stale_entries(&ran, &diverged)
        .into_iter()
        .map(String::from)
        .collect();
    assert_eq!(stale_entries, vec!["TEST-STALE".to_string()]);

    let report = RunReport {
        outcomes: vec![CaseOutcome {
            case_id: FABRICATED_CASE_ID,
            suite_name: "test",
            verdict,
        }],
        stale_entries,
        oracle_version: "0.48.0".to_string(),
    };
    assert_eq!(
        report.exit_code(),
        1,
        "a stale entry must fail the run even though the one case in it Matched"
    );
}
