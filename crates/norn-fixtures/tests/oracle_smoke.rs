//! Skip-if-absent oracle smoke test. `norn` (the parity oracle, ADR 0018)
//! is not necessarily on PATH in every environment `cargo test` runs in —
//! notably not in CI's `cargo test` step, which runs before the oracle
//! install step (see `.github/workflows/ci.yml`'s explicit `fixture-gen
//! smoke (oracle)` step for the CI-side equivalent of this check). Locally,
//! with `norn` installed, this test runs for real and is load-bearing: the
//! `clean` profile MUST validate clean against the real oracle.
//!
//! Empirically-discovered oracle behavior this test works around: `norn`
//! treats a vault root whose own basename starts with `.` as fully hidden
//! (an empty graph — 0 docs), the same way it hides dotfiles *inside* a
//! vault. `tempfile::TempDir::new()` on this platform creates dot-prefixed
//! directory names (e.g. `.tmpXXXXXX`), so generating straight into
//! `TempDir::new()`'s path and validating it against the oracle silently
//! sees an empty vault. The fix is to generate into a non-dot-prefixed
//! `vault/` subdirectory of the temp dir — confirmed the oracle only
//! applies the hidden check to path components *inside* the `-C` root, not
//! to the root's own name or its ancestors.

use std::path::PathBuf;
use std::process::Command;

use norn_fixtures::Profile;
use tempfile::TempDir;

fn oracle_present() -> bool {
    Command::new("norn")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn generate(profile: &Profile, seed: u64) -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let vault = dir.path().join("vault");
    norn_fixtures::generate(profile, seed, &vault).unwrap();
    (dir, vault)
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
fn clean_profile_validates_clean_against_the_oracle() {
    if !oracle_present() {
        eprintln!("skip: `norn` not found on PATH — oracle_smoke skipped");
        return;
    }

    let (_dir, vault) = generate(&Profile::clean(), 1);
    let stdout = validate_json(&vault);
    assert!(
        stdout.contains("\"findings\": 0"),
        "expected clean profile to validate with zero findings, got:\n{stdout}"
    );
}

#[test]
fn zoo_profile_reports_the_expected_finding_codes() {
    if !oracle_present() {
        eprintln!("skip: `norn` not found on PATH — oracle_smoke skipped");
        return;
    }

    let (_dir, vault) = generate(&Profile::zoo(), 1);
    let stdout = validate_json(&vault);

    assert!(
        !stdout.contains("\"findings\": 0"),
        "expected zoo profile to report findings, got:\n{stdout}"
    );

    for code in [
        "value-not-allowed",
        "frontmatter-parse-failed",
        "field-type-invalid",
        "link-target-missing",
        "document-misrouted",
        "frontmatter-forbidden-field",
        "frontmatter-required-field-missing",
    ] {
        assert!(
            stdout.contains(code),
            "expected zoo profile findings to mention {code}, got:\n{stdout}"
        );
    }
}
