//! MCP frame driving (NRN-383): the real `run::run_suites` orchestration
//! over a synthetic one-case MCP catalog, driven by executable STUB scripts
//! (a stand-in for `norn mcp`) — mirroring `tests/mutation.rs`'s pattern for
//! the mutating-case machinery. These tests need no installed oracle.
//!
//! Stubs are `/bin/sh` scripts. Each answers `--version` with a pinned
//! semver token (satisfying the harness's oracle-version pin) and, given
//! `mcp` as `$1`, drains stdin then writes canned newline-delimited
//! JSON-RPC response lines — the same framing `crate::mcp`'s doc confirms
//! empirically against the real oracle.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use norn_parity::cases::{Case, Fixture, Suite};
use norn_parity::run::{self, Mode, RunConfig, RunError};
use norn_parity::Verdict;

const CLEAN_1: Fixture = Fixture {
    profile_name: "clean",
    seed: 1,
};

const MCP_CASE_ID: &str = "fab-mcp-clean";

const REQUEST_FRAMES: &[&str] = &[
    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0.0.1"}}}"#,
    r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
];

/// Write `body` as an executable `/bin/sh` script at `dir/name`.
fn write_stub(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

/// The `--version` preamble every stub shares, plus dispatch on `mcp` vs.
/// anything else (any non-mcp/non-version argv exits 0 quietly — this
/// crate's cases never invoke the stub any other way for this suite).
const STUB_PREAMBLE: &str = "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"stub 9.9.9\"; exit 0; fi\nif [ \"$1\" != \"mcp\" ]; then exit 0; fi\ncat > /dev/null\n";

/// A stub that drains stdin then answers both request frames (ids 1 and 2)
/// with a `serverInfo.version` of `version` and a `tools/list` result body
/// of `tools_result` verbatim, exit 0.
fn mcp_stub_body(version: &str, tools_result: &str) -> String {
    format!(
        "{STUB_PREAMBLE}echo '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"protocolVersion\":\"2024-11-05\",\"serverInfo\":{{\"name\":\"norn\",\"version\":\"{version}\"}},\"capabilities\":{{\"tools\":{{}}}}}}}}'\necho '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"tools\":{tools_result}}}}}'\nexit 0\n"
    )
}

/// A stub that drains stdin, answers ONLY id 1 (never id 2), then exits —
/// the premature-EOF shape.
fn eof_early_stub_body() -> String {
    format!(
        "{STUB_PREAMBLE}echo '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"protocolVersion\":\"2024-11-05\",\"serverInfo\":{{\"name\":\"norn\",\"version\":\"9.9.9\"}},\"capabilities\":{{\"tools\":{{}}}}}}}}'\nexit 0\n"
    )
}

/// A stub that drains stdin then sleeps far longer than the harness's MCP
/// timeout before ever answering — the runner must kill it and time out
/// rather than hang. `exec sleep 30` (not a plain `sleep 30`, which forks a
/// child that would keep the shell's stdout pipe fd open even after the
/// shell itself is killed) REPLACES the shell's own process image, so
/// killing this stub's pid genuinely terminates the sleeping process and
/// closes its pipe ends immediately.
fn hanging_stub_body() -> String {
    format!("{STUB_PREAMBLE}exec sleep 30\n")
}

/// A one-case MCP catalog: `argv: ["mcp"]`, `stdin: Some(REQUEST_FRAMES)`,
/// `ported: true` so `Mode::Gated` selects it.
fn one_case_suite() -> Vec<Suite> {
    vec![Suite {
        name: "fabricated",
        cases: Box::leak(Box::new([Case {
            id: MCP_CASE_ID,
            argv: &["mcp"],
            fixture: CLEAN_1,
            stdin: Some(REQUEST_FRAMES),
            mutating: false,
            ported: true,
            expect_oracle_exit: 0,
            requires_doc: None,
            requires_code: None,
            normalize: &[],
        }])),
    }]
}

fn write_ledger_meta_only(path: &Path) {
    common::write_ledger(path, "[meta]\noracle_version = \"9.9.9\"\n");
}

#[test]
fn identical_frames_modulo_normalized_fields_match() {
    let bin_dir = tempfile::TempDir::new().unwrap();
    // Different server versions on each side — a raw diff, but
    // `crate::mcp`'s `serverInfo.version` normalization must neutralize it,
    // so the overall verdict is still Match.
    let oracle = write_stub(bin_dir.path(), "oracle", &mcp_stub_body("9.9.9", "[]"));
    let candidate = write_stub(bin_dir.path(), "candidate", &mcp_stub_body("8.8.8", "[]"));

    let ledger_dir = tempfile::TempDir::new().unwrap();
    let ledger_path = ledger_dir.path().join("ledger.toml");
    write_ledger_meta_only(&ledger_path);

    let suites = one_case_suite();
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
        "differing serverInfo.version alone must normalize away to a Match"
    );
    assert!(
        report.outcomes[0].mcp_diffs.is_empty(),
        "a Match must carry no reported frame diffs"
    );
}

#[test]
fn a_differing_frame_with_no_ledger_entry_is_drift_with_the_right_pointer() {
    let bin_dir = tempfile::TempDir::new().unwrap();
    let oracle = write_stub(bin_dir.path(), "oracle", &mcp_stub_body("9.9.9", "[]"));
    // The candidate's tools/list result genuinely differs (not just the
    // normalized version field): `tools: []` vs. one entry.
    let candidate = write_stub(
        bin_dir.path(),
        "candidate",
        &mcp_stub_body("9.9.9", r#"[{"name":"vault.other"}]"#),
    );

    let ledger_dir = tempfile::TempDir::new().unwrap();
    let ledger_path = ledger_dir.path().join("ledger.toml");
    write_ledger_meta_only(&ledger_path);

    let suites = one_case_suite();
    let config = RunConfig {
        mode: Mode::Gated,
        oracle: &oracle,
        rewrite: &candidate,
        ledger_path: &ledger_path,
        suite_filter: &[],
    };
    let report = run::run_suites(&config, Box::leak(suites.into_boxed_slice())).unwrap();

    assert_eq!(report.outcomes.len(), 1);
    assert_eq!(report.outcomes[0].verdict, Verdict::Drift);
    let diffs = &report.outcomes[0].mcp_diffs;
    assert_eq!(diffs.len(), 1, "exactly one response frame differs");
    assert_eq!(diffs[0].id, "2");
    assert_eq!(diffs[0].method, "tools/list");
    assert_eq!(
        diffs[0].pointer, "/result/tools/0",
        "the pointer must lead to the first differing element, not a full dump"
    );
}

#[test]
fn a_differing_frame_covered_by_an_entry_is_diverged_citing_it() {
    let bin_dir = tempfile::TempDir::new().unwrap();
    let oracle = write_stub(bin_dir.path(), "oracle", &mcp_stub_body("9.9.9", "[]"));
    let candidate = write_stub(
        bin_dir.path(),
        "candidate",
        &mcp_stub_body("9.9.9", r#"[{"name":"vault.other"}]"#),
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
id = "TEST-MCP-DIVERGED"
surface = "mcp (fabricated)"
cases = ["{MCP_CASE_ID}"]
old = "tools: []"
new = "tools: [vault.other]"
reason = "decided-better"
decision = "docs/decisions/0018-greenfield-rewrite-oracle-parity.md"
"#
        ),
    );

    let suites = one_case_suite();
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
            entry_id: "TEST-MCP-DIVERGED".to_string()
        }
    );
    assert!(report.stale_entries.is_empty());
}

#[test]
fn a_response_timeout_is_a_runner_error_never_a_verdict() {
    let bin_dir = tempfile::TempDir::new().unwrap();
    // The oracle hangs past the harness's MCP timeout; the candidate is
    // never even reached.
    let oracle = write_stub(bin_dir.path(), "oracle", &hanging_stub_body());
    let candidate = write_stub(bin_dir.path(), "candidate", &mcp_stub_body("9.9.9", "[]"));

    let ledger_dir = tempfile::TempDir::new().unwrap();
    let ledger_path = ledger_dir.path().join("ledger.toml");
    write_ledger_meta_only(&ledger_path);

    let suites = one_case_suite();
    let config = RunConfig {
        mode: Mode::Gated,
        oracle: &oracle,
        rewrite: &candidate,
        ledger_path: &ledger_path,
        suite_filter: &[],
    };

    let started = Instant::now();
    let result = run::run_suites(&config, Box::leak(suites.into_boxed_slice()));
    let elapsed = started.elapsed();

    match result {
        Err(RunError::Mcp { case_id, .. }) => assert_eq!(case_id, MCP_CASE_ID),
        Err(other) => {
            panic!("expected RunError::Mcp on a hung oracle, got a different error: {other}")
        }
        Ok(report) => panic!(
            "expected RunError::Mcp on a hung oracle, got a report (exit {})",
            report.exit_code()
        ),
    }
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "the runner must kill-and-reap on its own bounded timeout, not wait for the \
         stub's full 30s sleep; took {elapsed:?}"
    );
}

#[test]
fn a_premature_eof_is_a_runner_error_never_a_verdict() {
    let bin_dir = tempfile::TempDir::new().unwrap();
    let oracle = write_stub(bin_dir.path(), "oracle", &mcp_stub_body("9.9.9", "[]"));
    // The candidate answers id 1 then exits — id 2 is never answered.
    let candidate = write_stub(bin_dir.path(), "candidate", &eof_early_stub_body());

    let ledger_dir = tempfile::TempDir::new().unwrap();
    let ledger_path = ledger_dir.path().join("ledger.toml");
    write_ledger_meta_only(&ledger_path);

    let suites = one_case_suite();
    let config = RunConfig {
        mode: Mode::Gated,
        oracle: &oracle,
        rewrite: &candidate,
        ledger_path: &ledger_path,
        suite_filter: &[],
    };

    let result = run::run_suites(&config, Box::leak(suites.into_boxed_slice()));
    match result {
        Err(RunError::Mcp { case_id, .. }) => assert_eq!(case_id, MCP_CASE_ID),
        Err(other) => {
            panic!("expected RunError::Mcp on a premature EOF, got a different error: {other}")
        }
        Ok(report) => panic!(
            "expected RunError::Mcp on a premature EOF, got a report (exit {})",
            report.exit_code()
        ),
    }
}
