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
        .prefix("norn-skip-reason-")
        .tempdir()
        .expect("temp dir should be created");
    // Keep the directory on disk — we manage lifetime manually.
    let path = dir.path().to_path_buf();
    std::mem::forget(dir);
    path
}

/// Runs `vault` with the given args. Isolates the XDG cache and state trees
/// so tests don't share SQLite state and never read or sweep the developer's
/// real cache/state trees.
fn vault_json(args: &[&str]) -> Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.args(args);
    // Isolate cache + state per run.
    let cache_dir = tempfile::Builder::new()
        .prefix("norn-cache-")
        .tempdir()
        .expect("cache temp dir should be created");
    command.env("XDG_CACHE_HOME", cache_dir.path());
    command.env("XDG_STATE_HOME", cache_dir.path().join("state"));

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
        "--plan",
        "--format",
        "json",
    ];

    // --- Baseline: no --skip-reason filter ---
    // `norn repair --plan` now emits a MigrationPlan: `operations` + `skipped`,
    // where each skipped entry carries the kebab-case reason code in `reason`.
    let unfiltered = vault_json(&base_args);

    let unfiltered_skipped = unfiltered["skipped"]
        .as_array()
        .expect("skipped should be array");
    // Both kebab-case reason codes should appear.
    let unfiltered_reasons: Vec<&str> = unfiltered_skipped
        .iter()
        .map(|f| f["reason"].as_str().expect("reason should be str"))
        .collect();
    assert!(
        unfiltered_reasons.contains(&"missing-default"),
        "baseline should contain missing-default; got: {unfiltered_reasons:?}"
    );
    assert!(
        unfiltered_reasons.contains(&"link-decision-needed"),
        "baseline should contain link-decision-needed; got: {unfiltered_reasons:?}"
    );

    // Every skipped entry must carry a finding_code and a non-empty reason code.
    for entry in unfiltered_skipped {
        assert!(
            !entry["reason"].as_str().unwrap_or("").is_empty(),
            "reason must not be empty; entry: {entry}"
        );
        assert!(
            !entry["finding_code"].as_str().unwrap_or("").is_empty(),
            "finding_code must not be empty; entry: {entry}"
        );
    }

    let unfiltered_ops_count = unfiltered["operations"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    let unfiltered_skipped_total = unfiltered_skipped.len();

    // --- Filtered: --skip-reason missing-default ---
    let filtered_args: Vec<&str> = base_args
        .iter()
        .copied()
        .chain(["--skip-reason", "missing-default"])
        .collect();
    let filtered = vault_json(&filtered_args);

    // operations count must be unchanged — --skip-reason narrows skipped only.
    let filtered_ops_count = filtered["operations"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(
        filtered_ops_count, unfiltered_ops_count,
        "operations count must not change after --skip-reason filter"
    );

    // skipped narrowed to only missing-default entries.
    let filtered_skipped = filtered["skipped"]
        .as_array()
        .expect("skipped should be array");
    assert!(
        !filtered_skipped.is_empty(),
        "filtered skipped should have at least one entry"
    );
    for entry in filtered_skipped {
        assert_eq!(
            entry["reason"].as_str().expect("reason should be str"),
            "missing-default",
            "every entry after --skip-reason missing-default must have reason missing-default"
        );
    }

    // skipped total is strictly less than the unfiltered total (we filtered something out).
    assert!(
        filtered_skipped.len() < unfiltered_skipped_total,
        "filtered total ({}) should be less than unfiltered total ({unfiltered_skipped_total})",
        filtered_skipped.len()
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
        "--plan",
        "--format",
        "json",
    ];

    // No --skip-reason: both reason codes survive in the skipped set.
    let unfiltered = vault_json(&base_args);
    let skipped = unfiltered["skipped"]
        .as_array()
        .expect("skipped should be array");
    let reasons: Vec<&str> = skipped
        .iter()
        .map(|f| f["reason"].as_str().expect("reason should be str"))
        .collect();
    assert!(
        reasons.contains(&"missing-default") && reasons.contains(&"link-decision-needed"),
        "no --skip-reason flag should retain all skipped reasons; got: {reasons:?}"
    );

    fs::remove_dir_all(&root).ok();
    fs::remove_file(&config_path).ok();
}
