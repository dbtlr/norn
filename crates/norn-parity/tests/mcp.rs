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

/// Like [`mcp_stub_body`], but exits `exit_code` instead of 0 — F1: the
/// process exit code is part of the comparison even when every frame's
/// content is byte-identical.
fn mcp_stub_body_with_exit(version: &str, tools_result: &str, exit_code: i32) -> String {
    format!(
        "{STUB_PREAMBLE}echo '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"protocolVersion\":\"2024-11-05\",\"serverInfo\":{{\"name\":\"norn\",\"version\":\"{version}\"}},\"capabilities\":{{\"tools\":{{}}}}}}}}'\necho '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"tools\":{tools_result}}}}}'\nexit {exit_code}\n"
    )
}

/// Like [`mcp_stub_body`], but writes one EXTRA response line (`extra_line`,
/// a full JSON-RPC frame) for an id never requested — F2.
fn mcp_stub_body_with_extra(version: &str, tools_result: &str, extra_line: &str) -> String {
    format!(
        "{STUB_PREAMBLE}echo '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"protocolVersion\":\"2024-11-05\",\"serverInfo\":{{\"name\":\"norn\",\"version\":\"{version}\"}},\"capabilities\":{{\"tools\":{{}}}}}}}}'\necho '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"tools\":{tools_result}}}}}'\necho '{extra_line}'\nexit 0\n"
    )
}

/// Like [`mcp_stub_body`], but answers id 2 TWICE (identical content both
/// times) — F3.
fn mcp_stub_body_with_duplicate_id2(version: &str, tools_result: &str) -> String {
    format!(
        "{STUB_PREAMBLE}echo '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"protocolVersion\":\"2024-11-05\",\"serverInfo\":{{\"name\":\"norn\",\"version\":\"{version}\"}},\"capabilities\":{{\"tools\":{{}}}}}}}}'\necho '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"tools\":{tools_result}}}}}'\necho '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"tools\":{tools_result}}}}}'\nexit 0\n"
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

const PING_CASE_ID: &str = "fab-ping-clean";

/// A two-case catalog: the MCP case above, plus a plain non-MCP case whose
/// argv (`["ping"]`) the stub's preamble answers quietly (exit 0, no
/// output) on both sides regardless — so it always Matches. Used to prove
/// an MCP driving failure in `Mode::All` does not swallow the rest of the
/// burn-down (F4): this second case must still produce a normal row.
fn two_case_suite() -> Vec<Suite> {
    vec![Suite {
        name: "fabricated",
        cases: Box::leak(Box::new([
            Case {
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
            },
            Case {
                id: PING_CASE_ID,
                argv: &["ping"],
                fixture: CLEAN_1,
                stdin: None,
                mutating: false,
                ported: true,
                expect_oracle_exit: 0,
                requires_doc: None,
                requires_code: None,
                normalize: &[],
            },
        ])),
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
        report.outcomes[0].mcp_divergence.is_none(),
        "a Match must carry no reported divergence"
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
    let divergence = report.outcomes[0]
        .mcp_divergence
        .as_ref()
        .expect("a Drift outcome must carry the divergence detail");
    assert_eq!(
        divergence.diffs.len(),
        1,
        "exactly one response frame differs"
    );
    assert_eq!(divergence.diffs[0].id, "2");
    assert_eq!(divergence.diffs[0].method, "tools/list");
    assert_eq!(
        divergence.diffs[0].pointer, "/result/tools/0",
        "the pointer must lead to the first differing element, not a full dump"
    );
    assert!(
        divergence.exit_mismatch.is_none(),
        "both stubs exit 0 — only the frame content differs"
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

// ---- F1: candidate exit code is part of the comparison ---------------------

#[test]
fn a_candidate_exit_code_mismatch_with_identical_frames_is_drift_not_a_silent_match() {
    let bin_dir = tempfile::TempDir::new().unwrap();
    let oracle = write_stub(bin_dir.path(), "oracle", &mcp_stub_body("9.9.9", "[]"));
    // Every frame is byte-identical to the oracle's, but the process itself
    // exits non-zero — before F1 this would have been blessed as a silent
    // Match (the candidate exit was discarded).
    let candidate = write_stub(
        bin_dir.path(),
        "candidate",
        &mcp_stub_body_with_exit("9.9.9", "[]", 3),
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
    assert_eq!(
        report.outcomes[0].verdict,
        Verdict::Drift,
        "identical frame content but a differing exit code must not be a silent Match"
    );
    let divergence = report.outcomes[0]
        .mcp_divergence
        .as_ref()
        .expect("a Drift outcome must carry the divergence detail");
    assert_eq!(divergence.exit_mismatch, Some((0, 3)));
    assert!(
        divergence.diffs.is_empty(),
        "frame content genuinely matched — only the exit code differs"
    );
}

// ---- F2: an unsolicited response id is a divergence ------------------------

#[test]
fn an_unsolicited_response_id_is_a_divergence_naming_the_side() {
    let bin_dir = tempfile::TempDir::new().unwrap();
    let oracle = write_stub(bin_dir.path(), "oracle", &mcp_stub_body("9.9.9", "[]"));
    // The candidate additionally answers an id (99) that was never
    // requested, alongside correctly answering ids 1 and 2.
    let candidate = write_stub(
        bin_dir.path(),
        "candidate",
        &mcp_stub_body_with_extra(
            "9.9.9",
            "[]",
            r#"{"jsonrpc":"2.0","id":99,"result":{"unsolicited":true}}"#,
        ),
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

    assert_eq!(report.outcomes[0].verdict, Verdict::Drift);
    let divergence = report.outcomes[0].mcp_divergence.as_ref().unwrap();
    assert_eq!(divergence.extra_response_ids.len(), 1);
    assert_eq!(divergence.extra_response_ids[0].id, "99");
    assert_eq!(
        divergence.extra_response_ids[0].label, "rewrite",
        "the side that produced the unsolicited response must be named"
    );
    assert!(
        divergence.diffs.is_empty(),
        "the requested ids' content genuinely matched"
    );
}

// ---- F3: a repeated response id is a divergence ----------------------------

#[test]
fn a_repeated_response_id_is_a_divergence_naming_the_side_and_count() {
    let bin_dir = tempfile::TempDir::new().unwrap();
    let oracle = write_stub(bin_dir.path(), "oracle", &mcp_stub_body("9.9.9", "[]"));
    // The candidate answers id 2 twice (identical content both times) — a
    // repeat, not a content difference.
    let candidate = write_stub(
        bin_dir.path(),
        "candidate",
        &mcp_stub_body_with_duplicate_id2("9.9.9", "[]"),
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

    assert_eq!(report.outcomes[0].verdict, Verdict::Drift);
    let divergence = report.outcomes[0].mcp_divergence.as_ref().unwrap();
    assert_eq!(divergence.duplicate_response_ids.len(), 1);
    assert_eq!(divergence.duplicate_response_ids[0].id, "2");
    assert_eq!(divergence.duplicate_response_ids[0].label, "rewrite");
    assert_eq!(divergence.duplicate_response_ids[0].count, 2);
    assert!(
        divergence.diffs.is_empty(),
        "the repeated frames' content genuinely matched — only the repeat count is the anomaly"
    );
}

// ---- F4: --all renders an MCP driving error as a row, never aborts --------

#[test]
fn all_mode_renders_an_mcp_driving_error_as_a_runner_error_row_without_aborting() {
    let bin_dir = tempfile::TempDir::new().unwrap();
    let oracle = write_stub(bin_dir.path(), "oracle", &mcp_stub_body("9.9.9", "[]"));
    // The candidate never answers id 2 — a premature EOF, exactly the shape
    // the rewrite's not-yet-ported `mcp` subcommand produces today (it never
    // reads stdin at all).
    let candidate = write_stub(bin_dir.path(), "candidate", &eof_early_stub_body());

    let ledger_dir = tempfile::TempDir::new().unwrap();
    let ledger_path = ledger_dir.path().join("ledger.toml");
    write_ledger_meta_only(&ledger_path);

    let suites = two_case_suite();
    let config = RunConfig {
        mode: Mode::All,
        oracle: &oracle,
        rewrite: &candidate,
        ledger_path: &ledger_path,
        suite_filter: &[],
    };

    let report = run::run_suites(&config, Box::leak(suites.into_boxed_slice()))
        .expect("--all must render a runner-error row instead of aborting the whole run");

    assert_eq!(
        report.outcomes.len(),
        2,
        "both cases must produce a row — the MCP failure must not swallow the rest"
    );

    let mcp_outcome = report
        .outcomes
        .iter()
        .find(|o| o.case_id == MCP_CASE_ID)
        .unwrap();
    assert_eq!(
        mcp_outcome.verdict,
        Verdict::Drift,
        "a runner-error row is still one of the three sanctioned verdicts internally"
    );
    let message = mcp_outcome
        .runner_error
        .as_ref()
        .expect("the MCP driving error must be captured as runner_error, not propagated");
    assert!(
        message.contains("premature EOF")
            || message.contains("EofEarly")
            || message.contains("EOF"),
        "the runner_error message should name the premature-EOF condition, got: {message}"
    );

    let ping_outcome = report
        .outcomes
        .iter()
        .find(|o| o.case_id == PING_CASE_ID)
        .unwrap();
    assert_eq!(
        ping_outcome.verdict,
        Verdict::Match,
        "the rest of the burn-down must still run and report normally"
    );
    assert!(ping_outcome.runner_error.is_none());
}
