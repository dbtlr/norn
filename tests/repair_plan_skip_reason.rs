use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

/// Creates an isolated temp directory using a non-hidden prefix so the
/// walker does not treat it as hidden (`.tmp` prefix would be skipped).
///
/// Returns the path; the caller must call `fs::remove_dir_all` for cleanup
/// since we need the directory to persist beyond the `TempDir` scope.
fn vault_root() -> PathBuf {
    let dir = tempfile::Builder::new()
        .prefix("vault-cli-skip-reason-")
        .tempdir()
        .expect("temp dir should be created");
    // Keep the directory on disk — we manage lifetime manually.
    let path = dir.path().to_path_buf();
    std::mem::forget(dir);
    path
}

/// Runs `vault` with the given args. Isolates the XDG cache so tests don't
/// share SQLite state.
fn vault_json(args: &[&str]) -> Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vault"));
    command.args(args);
    // Isolate cache per run.
    let cache_dir = tempfile::Builder::new()
        .prefix("vault-cli-cache-")
        .tempdir()
        .expect("cache temp dir should be created");
    command.env("XDG_CACHE_HOME", cache_dir.path());

    let output = command.output().expect("vault command should run");
    assert!(
        output.status.success(),
        "vault command failed\nargs: {:?}\nstdout:\n{}\nstderr:\n{}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    serde_json::from_str::<Value>(&stdout).expect("output should be valid JSON")
}

/// Builds a vault fixture that produces two distinct skip-reason kinds:
///
/// 1. `missing_default` — a doc has a required field (`status`) missing and
///    there is no repair rule providing a default.
/// 2. `link_decision_needed` — a doc has a broken wikilink (`[[totally-unknown-xyz]]`)
///    that has no closest-match candidate.
///
/// Returns `(root_dir, config_path)`.  Caller is responsible for cleanup.
fn build_two_skip_reason_fixture() -> (PathBuf, PathBuf) {
    let root = vault_root();
    let config_path = root.with_extension("yaml");

    // Config: require `status` on all docs; no repair rules (so MissingDefault is the skip path).
    fs::write(
        &config_path,
        "validate:\n  required_frontmatter:\n    - status\n  rules: []\nrepair:\n  rules: []\n",
    )
    .expect("config should write");

    // Doc 1: missing `status` field → RequiredFrontmatterMissing → MissingDefault skip reason.
    fs::write(
        root.join("alpha.md"),
        "---\ntitle: Alpha\n---\n# Alpha\n\nNo status field.\n",
    )
    .expect("alpha.md should write");

    // Doc 2: broken wikilink with a unique name that won't match any stem →
    // link-target-missing with no closest match → LinkDecisionNeeded skip reason.
    fs::write(
        root.join("beta.md"),
        "---\ntitle: Beta\nstatus: active\n---\n# Beta\n\n[[totally-unknown-xyzzy-12345]]\n",
    )
    .expect("beta.md should write");

    (root, config_path)
}

#[test]
fn skip_reason_filter_narrows_skipped_findings_only() {
    let (root, config_path) = build_two_skip_reason_fixture();

    let base_args = [
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "repair",
        "plan",
        "--format",
        "json",
    ];

    // --- Baseline: no --skip-reason filter ---
    let unfiltered = vault_json(&base_args);

    let unfiltered_skipped = unfiltered["skipped_findings"]
        .as_array()
        .expect("skipped_findings should be array");
    // Should have both skip reasons present.
    let unfiltered_reasons: Vec<&str> = unfiltered_skipped
        .iter()
        .map(|f| {
            f["skip_reason"]
                .as_str()
                .expect("skip_reason should be str")
        })
        .collect();
    assert!(
        unfiltered_reasons.contains(&"missing_default"),
        "baseline should contain missing_default; got: {unfiltered_reasons:?}"
    );
    assert!(
        unfiltered_reasons.contains(&"link_decision_needed"),
        "baseline should contain link_decision_needed; got: {unfiltered_reasons:?}"
    );

    // Every skipped finding must have a reason_code field (kebab-case).
    for entry in unfiltered_skipped {
        let reason_code = entry["reason_code"]
            .as_str()
            .expect("reason_code should be present as a string");
        assert!(
            !reason_code.is_empty(),
            "reason_code must not be empty; entry: {entry}"
        );
    }
    // The two expected reason codes (kebab-case) should both appear.
    let unfiltered_reason_codes: Vec<&str> = unfiltered_skipped
        .iter()
        .map(|f| {
            f["reason_code"]
                .as_str()
                .expect("reason_code should be str")
        })
        .collect();
    assert!(
        unfiltered_reason_codes.contains(&"missing-default"),
        "baseline reason_codes should contain missing-default; got: {unfiltered_reason_codes:?}"
    );
    assert!(
        unfiltered_reason_codes.contains(&"link-decision-needed"),
        "baseline reason_codes should contain link-decision-needed; got: {unfiltered_reason_codes:?}"
    );

    let unfiltered_findings_count = unfiltered["summary"]["findings"]
        .as_u64()
        .expect("findings should be u64");
    let unfiltered_changes_count = unfiltered["summary"]["planned_changes"]
        .as_u64()
        .expect("planned_changes should be u64");
    let unfiltered_skipped_total = unfiltered["summary"]["skipped"]["total"]
        .as_u64()
        .expect("skipped.total should be u64");
    assert_eq!(
        unfiltered_skipped_total,
        unfiltered_skipped.len() as u64,
        "summary.skipped.total should equal skipped_findings length"
    );

    // --- Filtered: --skip-reason missing-default ---
    let filtered_args: Vec<&str> = base_args
        .iter()
        .copied()
        .chain(["--skip-reason", "missing-default"])
        .collect();
    let filtered = vault_json(&filtered_args);

    // findings count must be unchanged.
    assert_eq!(
        filtered["summary"]["findings"]
            .as_u64()
            .expect("findings should be u64"),
        unfiltered_findings_count,
        "findings count must not change after --skip-reason filter"
    );

    // planned_changes must be unchanged.
    assert_eq!(
        filtered["summary"]["planned_changes"]
            .as_u64()
            .expect("planned_changes should be u64"),
        unfiltered_changes_count,
        "planned_changes must not change after --skip-reason filter"
    );

    // skipped_findings narrowed to only missing_default entries.
    let filtered_skipped = filtered["skipped_findings"]
        .as_array()
        .expect("skipped_findings should be array");
    assert!(
        !filtered_skipped.is_empty(),
        "filtered skipped_findings should have at least one entry"
    );
    for entry in filtered_skipped {
        assert_eq!(
            entry["skip_reason"]
                .as_str()
                .expect("skip_reason should be str"),
            "missing_default",
            "every entry after --skip-reason missing-default must have skip_reason missing_default"
        );
        assert_eq!(
            entry["reason_code"]
                .as_str()
                .expect("reason_code should be str"),
            "missing-default",
            "every entry after --skip-reason missing-default must have reason_code missing-default"
        );
    }

    // source_filters echoes the input back.
    let source_skip_reasons = filtered["source_filters"]["skip_reason"]
        .as_array()
        .expect("source_filters.skip_reason should be array");
    assert_eq!(
        source_skip_reasons.len(),
        1,
        "source_filters.skip_reason should have exactly one entry"
    );
    assert_eq!(
        source_skip_reasons[0]
            .as_str()
            .expect("element should be str"),
        "missing-default"
    );

    // summary.skipped.total matches filtered set.
    let filtered_total = filtered["summary"]["skipped"]["total"]
        .as_u64()
        .expect("skipped.total should be u64");
    assert_eq!(
        filtered_total,
        filtered_skipped.len() as u64,
        "summary.skipped.total must equal filtered skipped_findings length"
    );

    // summary.skipped.by_reason only has keys in the filtered set.
    let by_reason = filtered["summary"]["skipped"]["by_reason"]
        .as_object()
        .expect("by_reason should be object");
    for key in by_reason.keys() {
        assert_eq!(
            key, "missing-default",
            "by_reason should only contain missing-default after filter"
        );
    }

    // skipped total is strictly less than the unfiltered total (we filtered something out).
    assert!(
        filtered_total < unfiltered_skipped_total,
        "filtered total ({filtered_total}) should be less than unfiltered total ({unfiltered_skipped_total})"
    );

    // Cleanup
    fs::remove_dir_all(&root).ok();
    fs::remove_file(&config_path).ok();
}

#[test]
fn skip_reason_filter_empty_list_returns_all_skipped() {
    let (root, config_path) = build_two_skip_reason_fixture();

    let base_args = [
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "repair",
        "plan",
        "--format",
        "json",
    ];

    let unfiltered = vault_json(&base_args);

    // No --skip-reason: source_filters.skip_reason should be empty list.
    let source_skip = unfiltered["source_filters"]["skip_reason"]
        .as_array()
        .expect("source_filters.skip_reason should be array");
    assert!(
        source_skip.is_empty(),
        "source_filters.skip_reason should be empty when no flag given"
    );

    fs::remove_dir_all(&root).ok();
    fs::remove_file(&config_path).ok();
}
