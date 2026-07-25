//! The environment spawned binaries run under (`norn_parity::exec::SpawnEnv`)
//! and the preflight that proves it held.
//!
//! A parity run is only entitled to measure the two binaries. Anything the
//! host contributes — a `norn serve` daemon the client finds through `$HOME`
//! and greets with a version-skew line, a `NORN_ROOT` pointing a case at
//! another vault — is measured as a difference and is invisible to the
//! verdicts that would otherwise catch it. These tests need no installed
//! oracle: both drive `/bin/sh` stubs.

mod common;

use common::write_stub;
use norn_parity::cases::{Case, Fixture, Suite};
use norn_parity::exec;
use norn_parity::run::{self, Mode, RunConfig, RunError};

const CLEAN_1: Fixture = Fixture {
    profile_name: "clean",
    seed: 1,
};

const CASE_ID: &str = "fab-env-clean";

fn one_case_suite() -> Vec<Suite> {
    vec![Suite {
        name: "fabricated",
        cases: Box::leak(Box::new([Case {
            id: CASE_ID,
            argv: &["count"],
            fixture: CLEAN_1,
            stdin: None,
            mutating: false,
            ported: true,
            expect_oracle_exit: 0,
            requires_doc: None,
            requires_code: None,
            normalize: &[],
            plan: None,
        }])),
    }]
}

#[test]
fn a_spawned_binary_sees_a_scratch_home_and_none_of_the_caller_environment() {
    let bin_dir = tempfile::TempDir::new().unwrap();
    // `CARGO_PKG_NAME` is set in every `cargo test` process, so its absence in
    // the child is proof the caller's environment was cleared rather than
    // inherited.
    let stub = write_stub(
        bin_dir.path(),
        "reporter",
        "#!/bin/sh\nprintf 'HOME=%s\\n' \"$HOME\"\nprintf 'XDG_CACHE_HOME=%s\\n' \"$XDG_CACHE_HOME\"\nprintf 'CARGO_PKG_NAME=%s\\n' \"${CARGO_PKG_NAME-<unset>}\"\nprintf 'PATH_SET=%s\\n' \"${PATH:+yes}\"\nexit 0\n",
    );

    let scratch = common::scratch_env();
    let vault = tempfile::TempDir::new().unwrap();
    let out =
        exec::run_argv(&stub, &[], None, vault.path(), &scratch.env).expect("the stub should run");
    let stdout = String::from_utf8(out.stdout).unwrap();

    let home_line = stdout
        .lines()
        .find_map(|l| l.strip_prefix("HOME="))
        .expect("the stub reports HOME");
    assert!(
        home_line.ends_with("scratch-home"),
        "HOME must point into the run's own scratch tree, got {home_line}"
    );
    assert_ne!(
        Some(home_line.to_string()),
        std::env::var("HOME").ok(),
        "the caller's HOME must not reach a spawned binary"
    );
    assert!(
        stdout
            .lines()
            .any(|l| l.starts_with("XDG_CACHE_HOME=") && l.ends_with("scratch-cache")),
        "XDG_CACHE_HOME must point into the scratch tree, got:\n{stdout}"
    );
    assert!(
        stdout.contains("CARGO_PKG_NAME=<unset>"),
        "the caller's environment must be cleared, got:\n{stdout}"
    );
    assert!(
        stdout.contains("PATH_SET=yes"),
        "PATH is on the allowlist so a binary can still find what it shells out to, got:\n{stdout}"
    );
}

#[test]
fn an_oracle_that_writes_to_stderr_on_a_clean_fixture_aborts_the_run() {
    let bin_dir = tempfile::TempDir::new().unwrap();
    // A stand-in for the real shape: a host daemon the oracle greets with a
    // version-skew notice on stderr, on every invocation.
    let noisy = write_stub(
        bin_dir.path(),
        "oracle",
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"stub 9.9.9\"; exit 0; fi\necho \"norn: service is v9.9.9, client is v9.9.8 — restart the norn serve daemon\" >&2\nexit 0\n",
    );
    let quiet = write_stub(
        bin_dir.path(),
        "candidate",
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"stub 9.9.9\"; exit 0; fi\nexit 0\n",
    );

    let ledger_dir = tempfile::TempDir::new().unwrap();
    let ledger_path = ledger_dir.path().join("ledger.toml");
    common::write_ledger(&ledger_path, "[meta]\noracle_version = \"9.9.9\"\n");

    let suites = one_case_suite();
    let config = RunConfig {
        mode: Mode::Gated,
        oracle: &noisy,
        rewrite: &quiet,
        ledger_path: &ledger_path,
        suite_filter: &[],
    };
    let err = run::run_suites(&config, Box::leak(suites.into_boxed_slice()))
        .err()
        .expect("a noisy oracle must abort the run, not produce a report");

    assert!(
        matches!(err, RunError::ForeignEnvironment { .. }),
        "expected ForeignEnvironment, got {err:?}"
    );
    let message = err.to_string();
    assert!(
        message.contains("norn serve"),
        "the diagnostic must name the usual cause, got: {message}"
    );
    assert!(
        message.contains("restart the norn serve daemon"),
        "the diagnostic must quote what the oracle actually wrote, got: {message}"
    );
}

#[test]
fn a_quiet_oracle_passes_the_preflight() {
    let bin_dir = tempfile::TempDir::new().unwrap();
    let stub_body =
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"stub 9.9.9\"; exit 0; fi\nexit 0\n";
    let oracle = write_stub(bin_dir.path(), "oracle", stub_body);
    let candidate = write_stub(bin_dir.path(), "candidate", stub_body);

    let ledger_dir = tempfile::TempDir::new().unwrap();
    let ledger_path = ledger_dir.path().join("ledger.toml");
    common::write_ledger(&ledger_path, "[meta]\noracle_version = \"9.9.9\"\n");

    let suites = one_case_suite();
    let config = RunConfig {
        mode: Mode::Gated,
        oracle: &oracle,
        rewrite: &candidate,
        ledger_path: &ledger_path,
        suite_filter: &[],
    };
    let report = run::run_suites(&config, Box::leak(suites.into_boxed_slice()))
        .expect("a silent oracle must pass the preflight");
    assert_eq!(report.outcomes.len(), 1, "the one ported case still runs");
}
