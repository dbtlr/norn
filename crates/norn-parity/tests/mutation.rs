//! Mutation-case support (NRN-375): fresh per-case vaults and post-state tree
//! comparison folded into the three-verdict machinery.
//!
//! These tests need no installed oracle. Two layers are exercised:
//!
//! - `FixtureCache::materialize` directly — proving a mutating case gets a
//!   fresh per-case copy (isolation) while a read case shares one cached copy.
//! - The real `run::run_suites` orchestration over a synthetic one-case
//!   catalog driven by executable STUB scripts (a stand-in for the future
//!   mutating `norn` binaries), proving a post-state tree difference folds into
//!   the SAME match/diverged/drift decision as a stdout/stderr/exit difference,
//!   and that a non-mutating case never triggers the tree comparison.
//!
//! Stubs are `/bin/sh` scripts (this crate's tests already assume a unixy
//! environment — cf. `/bin/echo` in `tests/verdicts.rs`). Each answers
//! `--version` with a pinned semver token so the harness's oracle-version pin
//! is satisfiable, and otherwise writes into its cwd (the fixture vault).

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use norn_parity::cases::{Case, Fixture, Suite};
use norn_parity::fixtures::{FixtureCache, Side};
use norn_parity::report;
use norn_parity::run::{self, Mode, RunConfig, RunError};
use norn_parity::Verdict;

const CLEAN_1: Fixture = Fixture {
    profile_name: "clean",
    seed: 1,
};

const MUT_CASE_ID: &str = "fab-mutation-clean";

/// Write `body` as an executable `/bin/sh` script at `dir/name`.
fn write_stub(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

/// A stub that answers `--version` with the pinned token and otherwise writes
/// `content` verbatim into `mutation.md` in its cwd, exit 0.
fn mutating_stub_body(content: &str) -> String {
    format!(
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"stub 9.9.9\"; exit 0; fi\nprintf '%s' '{content}' > mutation.md\nexit 0\n"
    )
}

/// The `--version` preamble every stub shares.
const STUB_VERSION_PREAMBLE: &str =
    "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"stub 9.9.9\"; exit 0; fi\n";

/// A stub that creates an empty directory `emptied/` in its cwd (no file
/// writes, no stdout), exit 0 — for the directory-cleanup divergence case.
fn empty_dir_stub_body() -> String {
    format!("{STUB_VERSION_PREAMBLE}mkdir emptied\nexit 0\n")
}

/// A stub that does nothing but answer `--version` (no writes, no stdout).
fn noop_stub_body() -> String {
    format!("{STUB_VERSION_PREAMBLE}exit 0\n")
}

/// A stub that deletes its own vault (its cwd) before exiting, so the
/// post-state snapshot cannot read the tree — forcing the runner-error path.
fn self_deleting_stub_body() -> String {
    format!("{STUB_VERSION_PREAMBLE}d=$(pwd)\ncd /\nrm -rf \"$d\"\nexit 0\n")
}

/// A one-case catalog with the `mutating` flag set as given, running an argv
/// that is inert to the stub (the stub ignores it) over the clean fixture.
fn one_case_suite(mutating: bool) -> Vec<Suite> {
    vec![Suite {
        name: "fabricated",
        cases: Box::leak(Box::new([Case {
            id: MUT_CASE_ID,
            argv: &["mutate"],
            fixture: CLEAN_1,
            stdin: None,
            mutating,
            ported: true,
            expect_oracle_exit: 0,
            requires_doc: None,
            requires_code: None,
            normalize: &[],
            plan: None,
        }])),
    }]
}

fn write_ledger_meta_only(path: &Path) {
    common::write_ledger(path, "[meta]\noracle_version = \"9.9.9\"\n");
}

// ── FixtureCache::materialize: copy strategy ────────────────────────────────

#[test]
fn a_mutating_case_gets_a_fresh_copy_that_a_later_case_never_sees() {
    let mut cache = FixtureCache::new().unwrap();

    // Case A materializes a fresh vault and writes a contaminant into it.
    let case_a = cache
        .materialize(&CLEAN_1, Side::Oracle, Some("case-a"))
        .unwrap();
    let contaminant = case_a.path.join("contaminant.md");
    std::fs::write(&contaminant, "---\ntype: note\n---\n").unwrap();
    assert!(contaminant.is_file(), "case A wrote its contaminant");

    // Case B, same fixture + side, gets a DIFFERENT fresh copy — the
    // contaminant from case A is absent (fresh generation, not a shared tree).
    let case_b = cache
        .materialize(&CLEAN_1, Side::Oracle, Some("case-b"))
        .unwrap();
    assert_ne!(
        case_a.path, case_b.path,
        "each mutating case gets its own directory"
    );
    assert!(
        !case_b.path.join("contaminant.md").exists(),
        "case B must not observe case A's write"
    );
    assert!(
        contaminant.is_file(),
        "case A's own copy still carries the write (proving it was really written)"
    );
    // Both are real generated vaults (the clean profile emits documents).
    assert!(
        std::fs::read_dir(&case_b.path).unwrap().any(|e| e
            .unwrap()
            .path()
            .extension()
            .is_some_and(|x| x == "md")),
        "the fresh copy is a fully generated vault"
    );
}

#[test]
fn read_cases_share_one_cached_copy_per_side_but_mutating_copies_are_distinct() {
    let mut cache = FixtureCache::new().unwrap();

    let read_1 = cache.materialize(&CLEAN_1, Side::Oracle, None).unwrap();
    let read_2 = cache.materialize(&CLEAN_1, Side::Oracle, None).unwrap();
    assert_eq!(
        read_1.path, read_2.path,
        "a read case reuses the cached per-(fixture, side) copy"
    );

    // A different SIDE is a different copy; a mutating copy is different again.
    let candidate = cache.materialize(&CLEAN_1, Side::Candidate, None).unwrap();
    assert_ne!(read_1.path, candidate.path, "sides never share a copy");

    let mutating = cache
        .materialize(&CLEAN_1, Side::Oracle, Some(MUT_CASE_ID))
        .unwrap();
    assert_ne!(
        read_1.path, mutating.path,
        "a mutating case never reuses the shared read copy"
    );
}

// ── run_suites: post-state folded into the three verdicts ───────────────────

#[test]
fn identical_post_mutation_trees_match() {
    let bin_dir = tempfile::TempDir::new().unwrap();
    let oracle = write_stub(bin_dir.path(), "oracle", &mutating_stub_body("SAME"));
    let candidate = write_stub(bin_dir.path(), "candidate", &mutating_stub_body("SAME"));

    let ledger_dir = tempfile::TempDir::new().unwrap();
    let ledger_path = ledger_dir.path().join("ledger.toml");
    write_ledger_meta_only(&ledger_path);

    let suites = one_case_suite(true);
    let config = RunConfig {
        mode: Mode::Gated,
        oracle: &oracle,
        rewrite: &candidate,
        ledger_path: &ledger_path,
        suite_filter: &[],
    };
    let report = run::run_suites(&config, Box::leak(suites.into_boxed_slice())).unwrap();

    assert_eq!(report.outcomes.len(), 1);
    assert_eq!(
        report.outcomes[0].verdict,
        Verdict::Match,
        "both sides wrote identical bytes → equal trees → Match"
    );
    assert!(
        report.outcomes[0].post_state.is_none(),
        "a matching mutating case records no post-state diff"
    );
    assert_eq!(report.exit_code(), 0);
}

#[test]
fn divergent_post_mutation_trees_with_no_entry_are_drift() {
    let bin_dir = tempfile::TempDir::new().unwrap();
    let oracle = write_stub(
        bin_dir.path(),
        "oracle",
        &mutating_stub_body("ORACLE-WROTE-THIS"),
    );
    let candidate = write_stub(bin_dir.path(), "candidate", &mutating_stub_body("CAND"));

    let ledger_dir = tempfile::TempDir::new().unwrap();
    let ledger_path = ledger_dir.path().join("ledger.toml");
    write_ledger_meta_only(&ledger_path);

    let suites = one_case_suite(true);
    let leaked: &'static [Suite] = Box::leak(suites.into_boxed_slice());
    let config = RunConfig {
        mode: Mode::Gated,
        oracle: &oracle,
        rewrite: &candidate,
        ledger_path: &ledger_path,
        suite_filter: &[],
    };
    let report = run::run_suites(&config, leaked).unwrap();

    assert_eq!(
        report.outcomes[0].verdict,
        Verdict::Drift,
        "trees differ and no ledger entry covers it → Drift"
    );
    let diff = report.outcomes[0]
        .post_state
        .as_ref()
        .expect("a diverging mutating case records its tree diff");
    assert_eq!(
        diff.content_differs.len(),
        1,
        "mutation.md differs on both sides"
    );
    assert_eq!(diff.content_differs[0].path, "mutation.md");
    assert_eq!(report.exit_code(), 1);

    // The divergence is legible in the rendered report.
    let rendered = report::render(&report, Mode::Gated);
    assert!(
        rendered.contains("post-state divergences"),
        "report surfaces the post-state section, got:\n{rendered}"
    );
    assert!(
        rendered.contains("content differs: mutation.md"),
        "report names the differing path, got:\n{rendered}"
    );
}

#[test]
fn divergent_post_mutation_trees_covered_by_an_entry_are_diverged() {
    let bin_dir = tempfile::TempDir::new().unwrap();
    let oracle = write_stub(bin_dir.path(), "oracle", &mutating_stub_body("ORACLE"));
    let candidate = write_stub(
        bin_dir.path(),
        "candidate",
        &mutating_stub_body("CANDIDATE"),
    );

    let ledger_dir = tempfile::TempDir::new().unwrap();
    let ledger_path = ledger_dir.path().join("ledger.toml");
    common::write_ledger(
        &ledger_path,
        &format!(
            r#"
[meta]
oracle_version = "9.9.9"

[[entry]]
id = "TEST-POSTSTATE"
surface = "mutate (fabricated)"
cases = ["{MUT_CASE_ID}"]
old = "oracle writes ORACLE"
new = "candidate writes CANDIDATE"
reason = "decided-better"
decision = "docs/decisions/0018-greenfield-rewrite-oracle-parity.md"
"#
        ),
    );

    let suites = one_case_suite(true);
    let config = RunConfig {
        mode: Mode::Gated,
        oracle: &oracle,
        rewrite: &candidate,
        ledger_path: &ledger_path,
        suite_filter: &[],
    };
    let report = run::run_suites(&config, Box::leak(suites.into_boxed_slice())).unwrap();

    assert_eq!(
        report.outcomes[0].verdict,
        Verdict::Diverged {
            entry_id: "TEST-POSTSTATE".to_string()
        },
        "a post-state divergence a ledger entry cites is Diverged, not Drift"
    );
    assert!(
        report.stale_entries.is_empty(),
        "the entry's case diverged (on post-state), so it is not stale"
    );
    assert_eq!(report.exit_code(), 0);
}

#[test]
fn a_non_mutating_case_never_compares_trees() {
    // Both stubs WRITE divergent files, but the case is `mutating: false`, so
    // the tree is never snapshotted; only the identical stdout/stderr/exit
    // matter → Match.
    let bin_dir = tempfile::TempDir::new().unwrap();
    let oracle = write_stub(bin_dir.path(), "oracle", &mutating_stub_body("ORACLE-ONLY"));
    let candidate = write_stub(
        bin_dir.path(),
        "candidate",
        &mutating_stub_body("CAND-ONLY"),
    );

    let ledger_dir = tempfile::TempDir::new().unwrap();
    let ledger_path = ledger_dir.path().join("ledger.toml");
    write_ledger_meta_only(&ledger_path);

    let suites = one_case_suite(false);
    let config = RunConfig {
        mode: Mode::Gated,
        oracle: &oracle,
        rewrite: &candidate,
        ledger_path: &ledger_path,
        suite_filter: &[],
    };
    let report = run::run_suites(&config, Box::leak(suites.into_boxed_slice())).unwrap();

    assert_eq!(
        report.outcomes[0].verdict,
        Verdict::Match,
        "a read case ignores vault-tree state even when the stubs write differing files"
    );
    assert!(
        report.outcomes[0].post_state.is_none(),
        "a non-mutating case never records a post-state diff"
    );
    assert_eq!(report.exit_code(), 0);
}

#[test]
fn a_one_sided_empty_directory_diverges() {
    // Directory cleanup asymmetry: the oracle leaves an empty `emptied/` dir,
    // the candidate leaves none. Both produce identical (empty) stdout/stderr/
    // exit, so only the vault TREE differs — a file-only walk would call this
    // Match; the empty-dir marker makes it a divergence.
    let bin_dir = tempfile::TempDir::new().unwrap();
    let oracle = write_stub(bin_dir.path(), "oracle", &empty_dir_stub_body());
    let candidate = write_stub(bin_dir.path(), "candidate", &noop_stub_body());

    let ledger_dir = tempfile::TempDir::new().unwrap();
    let ledger_path = ledger_dir.path().join("ledger.toml");
    write_ledger_meta_only(&ledger_path);

    let suites = one_case_suite(true);
    let config = RunConfig {
        mode: Mode::Gated,
        oracle: &oracle,
        rewrite: &candidate,
        ledger_path: &ledger_path,
        suite_filter: &[],
    };
    let report = run::run_suites(&config, Box::leak(suites.into_boxed_slice())).unwrap();

    assert_eq!(
        report.outcomes[0].verdict,
        Verdict::Drift,
        "a one-sided empty directory is a tree divergence with no ledger entry → Drift"
    );
    let diff = report.outcomes[0]
        .post_state
        .as_ref()
        .expect("the empty-dir divergence is recorded");
    assert_eq!(
        diff.only_in_oracle,
        vec!["emptied/".to_string()],
        "the empty dir is present only on the oracle side"
    );
    assert_eq!(report.exit_code(), 1);

    let rendered = report::render(&report, Mode::Gated);
    assert!(
        rendered.contains("only in oracle: emptied/"),
        "report surfaces the one-sided empty directory, got:\n{rendered}"
    );
}

#[test]
fn a_snapshot_io_failure_is_a_runner_error_not_a_verdict() {
    // Each side deletes its own vault before exiting, so the post-state
    // snapshot cannot read the tree. That is an environment/IO failure, not a
    // verdict — the run aborts with `RunError::PostStateSnapshot` (the bin maps
    // it to exit 2), never a Match/Diverged/Drift.
    let bin_dir = tempfile::TempDir::new().unwrap();
    let stub_body = self_deleting_stub_body();
    let oracle = write_stub(bin_dir.path(), "oracle", &stub_body);
    let candidate = write_stub(bin_dir.path(), "candidate", &stub_body);

    let ledger_dir = tempfile::TempDir::new().unwrap();
    let ledger_path = ledger_dir.path().join("ledger.toml");
    write_ledger_meta_only(&ledger_path);

    let suites = one_case_suite(true);
    let config = RunConfig {
        mode: Mode::Gated,
        oracle: &oracle,
        rewrite: &candidate,
        ledger_path: &ledger_path,
        suite_filter: &[],
    };
    // `RunReport` is not `Debug`, so match explicitly rather than `expect_err`.
    match run::run_suites(&config, Box::leak(suites.into_boxed_slice())) {
        Err(RunError::PostStateSnapshot {
            case_id,
            side_label,
            ..
        }) => {
            assert_eq!(case_id, MUT_CASE_ID);
            assert_eq!(
                side_label, "oracle",
                "the oracle side is snapshotted first, so its failure surfaces"
            );
        }
        Err(other) => panic!("expected RunError::PostStateSnapshot, got: {other}"),
        Ok(_) => panic!("expected a runner error, got a completed run"),
    }
}
