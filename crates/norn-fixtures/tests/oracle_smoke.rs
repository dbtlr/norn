//! Skip-if-absent oracle smoke test. `norn` (the parity oracle, ADR 0018) is
//! installed before `cargo test` in CI (see `.github/workflows/ci.yml`), so
//! these run for real there and locally whenever `norn` is on PATH; they skip
//! cleanly when it is absent.
//!
//! The clean-family invariant — a profile with `violations: false` and no
//! injected breakage validates with zero oracle findings — is checked across
//! `clean`, `linky`, and `large`, each at seeds {1, 2, 7}. The zoo's expected
//! finding codes are read from the generated manifest (the single source), not
//! hardcoded here.
//!
//! Empirically-discovered oracle behavior the shared `generate_vault` helper
//! works around: `norn` treats a vault root whose own basename starts with `.`
//! as fully hidden (an empty graph), so the helper generates into a
//! non-dot-prefixed `vault/` subdirectory of the temp dir.

mod common;

use std::process::Command;

use common::generate_vault;
use norn_fixtures::Profile;

fn oracle_present() -> bool {
    Command::new("norn")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn validate_json(vault: &std::path::Path) -> String {
    let output = Command::new("norn")
        .args(["-C"])
        .arg(vault)
        .args(["validate", "--summary", "--format", "json"])
        .output()
        .expect("failed to run norn validate");
    assert!(
        output.status.success(),
        "norn validate exited non-zero: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

#[test]
fn clean_family_validates_clean_against_the_oracle() {
    if !oracle_present() {
        eprintln!("skip: `norn` not found on PATH — oracle_smoke skipped");
        return;
    }

    for name in ["clean", "linky", "large"] {
        let profile = Profile::by_name(name).unwrap();
        for seed in [1u64, 2, 7] {
            let (_dir, vault, _manifest) = generate_vault(&profile, seed);
            let stdout = validate_json(&vault);
            assert!(
                stdout.contains("\"findings\": 0"),
                "profile {name} seed {seed}: expected zero findings, got:\n{stdout}"
            );
        }
    }
}

#[test]
fn zoo_profile_reports_the_expected_finding_codes() {
    if !oracle_present() {
        eprintln!("skip: `norn` not found on PATH — oracle_smoke skipped");
        return;
    }

    let (_dir, vault, manifest) = generate_vault(&Profile::zoo(), 1);
    let stdout = validate_json(&vault);

    assert!(
        !stdout.contains("\"findings\": 0"),
        "expected zoo profile to report findings, got:\n{stdout}"
    );

    let expected: std::collections::BTreeSet<String> = manifest
        .expected_codes()
        .iter()
        .map(|c| c.to_string())
        .collect();
    assert!(
        !expected.is_empty(),
        "zoo manifest carried no expected finding codes"
    );
    let actual = parse_summary_codes(&stdout);
    assert_eq!(
        actual, expected,
        "oracle finding codes must equal the manifest's expected set — \
         a missing code means lost coverage, an extra code means an \
         unintended finding leaked into the zoo; got:\n{stdout}"
    );
}

/// Extract the code names from the summary JSON's `"codes": {{ ... }}` object
/// with plain string ops (the crate deliberately has no JSON dependency).
fn parse_summary_codes(stdout: &str) -> std::collections::BTreeSet<String> {
    let start = stdout
        .find("\"codes\": {")
        .expect("summary JSON should contain a codes object");
    let rest = &stdout[start + "\"codes\": {".len()..];
    let end = rest
        .find('}')
        .expect("summary codes object should be closed");
    rest[..end]
        .lines()
        .filter_map(|line| {
            let stripped = line.trim().strip_prefix('"')?;
            let (code, _) = stripped.split_once('"')?;
            Some(code.to_string())
        })
        .collect()
}
