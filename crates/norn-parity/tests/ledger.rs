//! Ledger parsing/validation tests. No oracle required — these exercise
//! `norn_parity::ledger` purely against TOML text and the crate's static
//! case catalog.

mod common;

use std::collections::BTreeSet;
use std::path::Path;

use norn_parity::ledger::{Ledger, LedgerError};

fn known_ids() -> BTreeSet<&'static str> {
    norn_parity::cases::all_case_ids().into_iter().collect()
}

/// For the structural tests that need their cited cases to *pass* the
/// ported-only rule, treat the whole catalog as ported. The dedicated
/// `rejects_an_entry_citing_an_unported_case` test passes a narrower set.
fn ported_ids() -> BTreeSet<&'static str> {
    norn_parity::cases::all_case_ids().into_iter().collect()
}

#[test]
fn parses_the_real_ledger_with_the_help_divergence_entries() {
    let path = common::workspace_root().join("docs/parity-ledger.toml");
    let ledger = Ledger::load(&path, &known_ids(), &ported_ids())
        .unwrap_or_else(|e| panic!("failed to load {}: {e}", path.display()));
    assert_eq!(ledger.meta.oracle_version, "0.48.0");
    // Phase 1 (NRN-329) added PD-101 (the `vault` namespace in top-level
    // `--help`). NRN-345 broadened PD-101 to also carry the GLOBAL OPTIONS
    // change on help-bare and added PD-102 for the same GLOBAL OPTIONS change on
    // the two ported subcommand help surfaces (help-validate / help-find).
    assert_eq!(
        ledger.entries.len(),
        2,
        "expected exactly PD-101 and PD-102, found {}",
        ledger.entries.len()
    );

    let pd101 = ledger
        .entry_for_case("help-bare")
        .expect("help-bare must resolve to an entry");
    assert_eq!(pd101.id, "PD-101");
    assert_eq!(pd101.reason, norn_parity::ledger::Reason::DecidedBetter);

    let pd102 = ledger
        .entry_for_case("help-validate")
        .expect("help-validate must resolve to an entry");
    assert_eq!(pd102.id, "PD-102");
    assert_eq!(
        ledger.entry_for_case("help-find").map(|e| e.id.as_str()),
        Some("PD-102"),
        "help-find shares PD-102 with help-validate"
    );
    assert_eq!(pd102.reason, norn_parity::ledger::Reason::DecidedBetter);
}

#[test]
fn rejects_an_unknown_reason() {
    let toml = r#"
[meta]
oracle_version = "0.48.0"

[[entry]]
id = "PD-001"
surface = "find --format json"
cases = ["help-bare"]
old = "old behavior"
new = "new behavior"
reason = "vibes"
decision = "docs/decisions/0018-greenfield-rewrite-oracle-parity.md"
"#;
    let err = Ledger::parse(toml, &known_ids(), &ported_ids()).unwrap_err();
    assert!(
        matches!(err, LedgerError::UnknownReason { .. }),
        "expected UnknownReason, got {err:?}"
    );
}

#[test]
fn rejects_an_entry_citing_no_cases() {
    let toml = r#"
[meta]
oracle_version = "0.48.0"

[[entry]]
id = "PD-001"
surface = "find --format json"
cases = []
old = "old behavior"
new = "new behavior"
reason = "decided-better"
decision = "docs/decisions/0018-greenfield-rewrite-oracle-parity.md"
"#;
    let err = Ledger::parse(toml, &known_ids(), &ported_ids()).unwrap_err();
    assert!(
        matches!(err, LedgerError::EmptyCases { .. }),
        "expected EmptyCases, got {err:?}"
    );
}

#[test]
fn rejects_a_duplicate_entry_id() {
    let toml = r#"
[meta]
oracle_version = "0.48.0"

[[entry]]
id = "PD-001"
surface = "a"
cases = ["help-bare"]
old = "old"
new = "new"
reason = "decided-better"
decision = "docs/decisions/0018-greenfield-rewrite-oracle-parity.md"

[[entry]]
id = "PD-001"
surface = "b"
cases = ["help-validate"]
old = "old"
new = "new"
reason = "decided-better"
decision = "docs/decisions/0018-greenfield-rewrite-oracle-parity.md"
"#;
    let err = Ledger::parse(toml, &known_ids(), &ported_ids()).unwrap_err();
    assert!(
        matches!(err, LedgerError::DuplicateEntryId(ref id) if id == "PD-001"),
        "expected DuplicateEntryId(\"PD-001\"), got {err:?}"
    );
}

#[test]
fn rejects_an_entry_citing_an_unknown_case_id() {
    let toml = r#"
[meta]
oracle_version = "0.48.0"

[[entry]]
id = "PD-001"
surface = "a"
cases = ["this-case-id-does-not-exist"]
old = "old"
new = "new"
reason = "decided-better"
decision = "docs/decisions/0018-greenfield-rewrite-oracle-parity.md"
"#;
    let err = Ledger::parse(toml, &known_ids(), &ported_ids()).unwrap_err();
    assert!(
        matches!(err, LedgerError::UnknownCaseId { ref case, .. } if case == "this-case-id-does-not-exist"),
        "expected UnknownCaseId, got {err:?}"
    );
}

#[test]
fn rejects_a_missing_required_field() {
    let toml = r#"
[meta]
oracle_version = "0.48.0"

[[entry]]
id = "PD-001"
surface = "a"
cases = ["help-bare"]
old = "old"
new = "new"
decision = "docs/decisions/0018-greenfield-rewrite-oracle-parity.md"
"#;
    let err = Ledger::parse(toml, &known_ids(), &ported_ids()).unwrap_err();
    assert!(
        matches!(
            err,
            LedgerError::MissingField {
                field: "reason",
                ..
            }
        ),
        "expected MissingField {{ field: \"reason\" }}, got {err:?}"
    );
}

#[test]
fn rejects_a_missing_meta_table() {
    let err = Ledger::parse("", &known_ids(), &ported_ids()).unwrap_err();
    assert!(
        matches!(err, LedgerError::MissingMetaTable),
        "expected MissingMetaTable, got {err:?}"
    );
}

#[test]
fn rejects_a_case_cited_by_more_than_one_entry() {
    let toml = r#"
[meta]
oracle_version = "0.48.0"

[[entry]]
id = "PD-001"
surface = "a"
cases = ["help-bare"]
old = "old"
new = "new"
reason = "decided-better"
decision = "docs/decisions/0018-greenfield-rewrite-oracle-parity.md"

[[entry]]
id = "PD-002"
surface = "b"
cases = ["help-bare"]
old = "old"
new = "new"
reason = "decided-better"
decision = "docs/decisions/0018-greenfield-rewrite-oracle-parity.md"
"#;
    let err = Ledger::parse(toml, &known_ids(), &ported_ids()).unwrap_err();
    assert!(
        matches!(err, LedgerError::CaseCitedByMultipleEntries { ref case, .. } if case == "help-bare"),
        "expected CaseCitedByMultipleEntries, got {err:?}"
    );
}

#[test]
fn rejects_an_entry_citing_an_unported_case() {
    // `help-bare` is a known case, but here it is NOT in the ported set — an
    // entry for an unported surface is premature (divergence can only be
    // observed once ported), so it must fail to load.
    let toml = r#"
[meta]
oracle_version = "0.48.0"

[[entry]]
id = "PD-001"
surface = "help-bare"
cases = ["help-bare"]
old = "old"
new = "new"
reason = "decided-better"
decision = "docs/decisions/0018-greenfield-rewrite-oracle-parity.md"
"#;
    let empty_ported: BTreeSet<&str> = BTreeSet::new();
    let err = Ledger::parse(toml, &known_ids(), &empty_ported).unwrap_err();
    assert!(
        matches!(err, LedgerError::UnportedCaseId { ref case, .. } if case == "help-bare"),
        "expected UnportedCaseId, got {err:?}"
    );
}

#[test]
fn accepts_a_well_formed_entry_and_resolves_it_by_case_id() {
    let toml = r#"
[meta]
oracle_version = "0.48.0"

[[entry]]
id = "PD-001"
surface = "help-bare"
cases = ["help-bare"]
old = "old behavior"
new = "new behavior"
reason = "discovered-inconsistency"
decision = "docs/decisions/0018-greenfield-rewrite-oracle-parity.md"
"#;
    let ledger =
        Ledger::parse(toml, &known_ids(), &ported_ids()).expect("well-formed ledger should parse");
    let entry = ledger
        .entry_for_case("help-bare")
        .expect("help-bare should resolve to PD-001");
    assert_eq!(entry.id, "PD-001");
    assert!(ledger.entry_for_case("help-validate").is_none());
}

#[test]
fn stale_entry_when_its_only_ran_case_matched() {
    let toml = r#"
[meta]
oracle_version = "0.48.0"

[[entry]]
id = "PD-001"
surface = "help-bare"
cases = ["help-bare"]
old = "old behavior"
new = "new behavior"
reason = "decided-better"
decision = "docs/decisions/0018-greenfield-rewrite-oracle-parity.md"
"#;
    let ledger = Ledger::parse(toml, &known_ids(), &ported_ids()).unwrap();

    let ran: BTreeSet<&str> = ["help-bare"].into_iter().collect();
    let diverged: BTreeSet<&str> = BTreeSet::new(); // nothing diverged: PD-001's case matched
    assert_eq!(ledger.stale_entries(&ran, &diverged), vec!["PD-001"]);
}

#[test]
fn not_stale_when_its_cited_case_diverged() {
    let toml = r#"
[meta]
oracle_version = "0.48.0"

[[entry]]
id = "PD-001"
surface = "help-bare"
cases = ["help-bare"]
old = "old behavior"
new = "new behavior"
reason = "decided-better"
decision = "docs/decisions/0018-greenfield-rewrite-oracle-parity.md"
"#;
    let ledger = Ledger::parse(toml, &known_ids(), &ported_ids()).unwrap();

    let ran: BTreeSet<&str> = ["help-bare"].into_iter().collect();
    let diverged: BTreeSet<&str> = ["help-bare"].into_iter().collect();
    assert!(ledger.stale_entries(&ran, &diverged).is_empty());
}

#[test]
fn not_stale_when_none_of_its_cases_ran_at_all() {
    let toml = r#"
[meta]
oracle_version = "0.48.0"

[[entry]]
id = "PD-001"
surface = "help-bare"
cases = ["help-bare"]
old = "old behavior"
new = "new behavior"
reason = "decided-better"
decision = "docs/decisions/0018-greenfield-rewrite-oracle-parity.md"
"#;
    let ledger = Ledger::parse(toml, &known_ids(), &ported_ids()).unwrap();

    // Suite filtering excluded help-bare from this run entirely — an entry
    // whose cases never ran cannot be judged stale.
    let ran: BTreeSet<&str> = ["read-count-clean"].into_iter().collect();
    let diverged: BTreeSet<&str> = BTreeSet::new();
    assert!(ledger.stale_entries(&ran, &diverged).is_empty());
}

#[test]
fn load_reports_the_file_path_on_a_missing_ledger() {
    let path = Path::new("/nonexistent/parity-ledger.toml");
    let err = Ledger::load(path, &known_ids(), &ported_ids()).unwrap_err();
    assert!(
        matches!(err, LedgerError::Io { .. }),
        "expected Io, got {err:?}"
    );
    let message = err.to_string();
    assert!(
        message.contains("/nonexistent/parity-ledger.toml"),
        "diagnostic should name the ledger path, got: {message}"
    );
}
