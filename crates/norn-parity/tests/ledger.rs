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
    assert_eq!(ledger.meta.oracle_version, "0.48.1");
    // Phase 1 (NRN-329) added PD-101 (the `vault` namespace in top-level
    // `--help`). NRN-345 broadened PD-101 to also carry the GLOBAL OPTIONS
    // change on help-bare and added PD-102 for the same GLOBAL OPTIONS change on
    // the two ported subcommand help surfaces (help-validate / help-find). The
    // text-layer slate (NRN-349 / NRN-350) added PD-103 (code-opacity block-id
    // resolution) and PD-104 (BOM-prefixed frontmatter); the CLI-semantics slate
    // added PD-105 (zero-indexed `--starts-at`, NRN-332) and PD-106 (last-wins
    // `--limit`/`--no-limit`, NRN-331); the URL-semantics slate added PD-107
    // (Markdown-link split-then-decode + block-ref, NRN-356) and PD-108
    // (external-vs-local scheme classification, NRN-357); the presentation/errors
    // slate added PD-109 (the soft-landing diagnostic surface, NRN-361/362) and
    // PD-110 (grammar-wide last-wins, NRN-365). The confirmed-apply mutation
    // slate (NRN-388) added PD-111 (the unified --format json warning envelope)
    // and PD-112 (the NRN-371 null-/comment-only frontmatter mapping-promotion,
    // two cases). NRN-405 added PD-113 (the authored-plan change-op kind/operation
    // mismatch refusal) and PD-114 (the malformed authored-plan refusal codes —
    // `unknown-operation-kind` / `malformed-plan` in place of `internal-error`,
    // two cases). NRN-437 added PD-115 (the SETEXT / heading-at-EOF section-op
    // corruption fix, five cases). NRN-424 added PD-116/117/118 (the
    // wikilink-rewriter unification: embed-marker, code-opacity, and caret-target
    // corruptions) and, on review, PD-119 (decided-better interior-whitespace
    // canonicalization on rewrite) and PD-120 (decided-better refuse/skip on a
    // rename to an unrepresentable wikilink target). NRN-406 (ADR 0022) added
    // PD-121 (decided-better strict op-payload decode: a wrong-typed op field
    // refuses `malformed-plan` instead of silently coercing, three cases) and
    // PD-122 (decided-better flat finding contract: `validate --format
    // json`/`jsonl` and `vault.validate` serialize one flat closed struct with
    // no leaked internal link/diagnostic model, three cases).
    assert_eq!(
        ledger.entries.len(),
        22,
        "expected exactly PD-101..PD-122, found {}",
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

    // The text-layer edge divergences (PD-103 / PD-104).
    let pd103 = ledger
        .entry_for_case("text-edge-code-fenced-block-id-link")
        .expect("the code-opacity case must resolve to an entry");
    assert_eq!(pd103.id, "PD-103");
    assert_eq!(pd103.reason, norn_parity::ledger::Reason::DecidedBetter);

    let pd104 = ledger
        .entry_for_case("text-edge-bom-doc-all-cols")
        .expect("the BOM case must resolve to an entry");
    assert_eq!(pd104.id, "PD-104");
    assert_eq!(
        pd104.reason,
        norn_parity::ledger::Reason::DiscoveredInconsistency
    );

    // The CLI-semantics divergences (PD-105 / PD-106).
    assert_eq!(
        ledger
            .entry_for_case("read-find-starts-at-zero-indexed-zoo")
            .map(|e| e.id.as_str()),
        Some("PD-105"),
        "the zero-indexed --starts-at case is gated by PD-105"
    );
    assert_eq!(
        ledger
            .entry_for_case("read-find-limit-nolimit-last-wins-zoo")
            .map(|e| e.id.as_str()),
        Some("PD-106"),
        "the last-wins --limit/--no-limit case is gated by PD-106"
    );

    // The URL-semantics divergences (PD-107 / PD-108).
    let pd107 = ledger
        .entry_for_case("url-edge-decode-split-blockref")
        .expect("the split-then-decode + block-ref case must resolve to an entry");
    assert_eq!(pd107.id, "PD-107");
    assert_eq!(pd107.reason, norn_parity::ledger::Reason::DecidedBetter);

    let pd108 = ledger
        .entry_for_case("url-edge-scheme-classification")
        .expect("the scheme-classification case must resolve to an entry");
    assert_eq!(pd108.id, "PD-108");
    assert_eq!(pd108.reason, norn_parity::ledger::Reason::DecidedBetter);

    // The presentation/errors divergences (PD-109 / PD-110). PD-109 (the
    // soft-landing diagnostic surface) covers three cases; PD-110 (grammar-wide
    // last-wins) one.
    for case in [
        "err-links-to-unresolvable-zoo",
        "err-col-facet-did-you-mean-zoo",
        "err-malformed-config",
    ] {
        assert_eq!(
            ledger.entry_for_case(case).map(|e| e.id.as_str()),
            Some("PD-109"),
            "{case} is gated by PD-109"
        );
    }
    let pd109 = ledger
        .entry_for_case("err-malformed-config")
        .expect("the malformed-config case must resolve to an entry");
    assert_eq!(pd109.reason, norn_parity::ledger::Reason::DecidedBetter);
    assert_eq!(
        ledger
            .entry_for_case("err-repeated-limit-last-wins-zoo")
            .map(|e| e.id.as_str()),
        Some("PD-110"),
        "the grammar-wide last-wins case is gated by PD-110"
    );

    // The NRN-405 authored-plan divergences (PD-113 / PD-114). PD-113 (the
    // change-op kind/operation mismatch refusal) is a decided-better safety
    // change; PD-114 (the malformed-plan refusal codes) covers three cases and is a
    // discovered-inconsistency (internal-error misclassified user-authored errors).
    let pd113 = ledger
        .entry_for_case("apply-authored-kind-operation-mismatch-refusal-zoo")
        .expect("the kind/operation mismatch case must resolve to an entry");
    assert_eq!(pd113.id, "PD-113");
    assert_eq!(pd113.reason, norn_parity::ledger::Reason::DecidedBetter);

    for case in [
        "apply-authored-unknown-kind-refusal-json-zoo",
        "apply-authored-missing-field-refusal-json-zoo",
        "apply-authored-wrong-typed-field-refusal-json-zoo",
    ] {
        assert_eq!(
            ledger.entry_for_case(case).map(|e| e.id.as_str()),
            Some("PD-114"),
            "{case} is gated by PD-114"
        );
    }
    let pd114 = ledger
        .entry_for_case("apply-authored-unknown-kind-refusal-json-zoo")
        .expect("the unknown-kind refusal case must resolve to an entry");
    assert_eq!(
        pd114.reason,
        norn_parity::ledger::Reason::DiscoveredInconsistency
    );

    // The NRN-437 section-op divergences (PD-115): the SETEXT / heading-at-EOF
    // corruption fix covers five `edit` cases under one entry.
    for case in [
        "edit-setext-replace-section-diverge",
        "edit-setext-insert-after-heading-diverge",
        "edit-eof-heading-replace-section-diverge",
        "edit-eof-heading-append-to-section-diverge",
        "edit-eof-heading-insert-after-heading-diverge",
    ] {
        assert_eq!(
            ledger.entry_for_case(case).map(|e| e.id.as_str()),
            Some("PD-115"),
            "{case} is gated by PD-115"
        );
    }
    let pd115 = ledger
        .entry_for_case("edit-setext-replace-section-diverge")
        .expect("the SETEXT replace_section case must resolve to an entry");
    assert_eq!(
        pd115.reason,
        norn_parity::ledger::Reason::DiscoveredInconsistency
    );

    // The NRN-424 wikilink-rewriter divergences: grouped by mechanism — PD-116
    // the embed-marker drop (a move case AND a delete --rewrite-to variant, same
    // cascade helpers), PD-117 the code-opacity fix (move cascade + rewrite-wikilink
    // verb), PD-118 the caret-target split, and PD-119 (decided-better) the
    // interior-whitespace canonicalization (a spaced-pipe move + a padded-target
    // rewrite-wikilink).
    for (case, entry) in [
        ("wl-move-embed-backlink-diverge", "PD-116"),
        ("wl-delete-embed-backlink-diverge", "PD-116"),
        ("wl-move-code-fence-shadow-diverge", "PD-117"),
        ("wl-rewrite-wikilink-code-fence-shadow-diverge", "PD-117"),
        ("wl-rewrite-wikilink-caret-stem-diverge", "PD-118"),
        ("wl-move-spaced-alias-diverge", "PD-119"),
        ("wl-rewrite-wikilink-padded-target-diverge", "PD-119"),
        ("wl-rewrite-wikilink-unrepresentable-refusal", "PD-120"),
        ("wl-move-unrepresentable-skip-diverge", "PD-120"),
    ] {
        assert_eq!(
            ledger.entry_for_case(case).map(|e| e.id.as_str()),
            Some(entry),
            "{case} is gated by {entry}"
        );
    }
    for case in [
        "wl-move-embed-backlink-diverge",
        "wl-move-code-fence-shadow-diverge",
        "wl-rewrite-wikilink-caret-stem-diverge",
    ] {
        assert_eq!(
            ledger.entry_for_case(case).unwrap().reason,
            norn_parity::ledger::Reason::DiscoveredInconsistency,
            "{case}'s entry is a discovered-inconsistency"
        );
    }
    // PD-119 and PD-120 are the branch's decided-better wikilink entries.
    for case in [
        "wl-move-spaced-alias-diverge",
        "wl-rewrite-wikilink-unrepresentable-refusal",
    ] {
        assert_eq!(
            ledger.entry_for_case(case).unwrap().reason,
            norn_parity::ledger::Reason::DecidedBetter,
            "{case}'s entry is decided-better"
        );
    }

    // The NRN-406 (ADR 0022) strict-decode divergences: three wrong-typed-op-field
    // refusals under one decided-better entry (PD-121).
    for case in [
        "apply-authored-wrong-typed-bool-refusal-zoo",
        "apply-authored-wrong-typed-rewrite-to-refusal-zoo",
        "apply-authored-wrong-typed-document-hash-refusal-zoo",
    ] {
        assert_eq!(
            ledger.entry_for_case(case).map(|e| e.id.as_str()),
            Some("PD-121"),
            "{case} is gated by PD-121"
        );
    }
    assert_eq!(
        ledger
            .entry_for_case("apply-authored-wrong-typed-bool-refusal-zoo")
            .unwrap()
            .reason,
        norn_parity::ledger::Reason::DecidedBetter,
        "PD-121 is decided-better"
    );

    // The NRN-406 (ADR 0022) flat-finding-contract divergences: the two CLI
    // finding shapes and the MCP structuredContent that follows them, all under
    // one decided-better entry (PD-122).
    for case in [
        "validate-code-filter-zoo",
        "validate-jsonl-code-zoo",
        "mcp-tools-call-validate-code-zoo",
    ] {
        assert_eq!(
            ledger.entry_for_case(case).map(|e| e.id.as_str()),
            Some("PD-122"),
            "{case} is gated by PD-122"
        );
    }
    assert_eq!(
        ledger
            .entry_for_case("validate-code-filter-zoo")
            .unwrap()
            .reason,
        norn_parity::ledger::Reason::DecidedBetter,
        "PD-122 is decided-better"
    );
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
