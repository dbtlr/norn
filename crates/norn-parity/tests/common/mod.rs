//! Shared test helpers. `tests/common/mod.rs` (not `tests/common.rs`) so
//! cargo treats it as a module included by each integration test rather
//! than its own test binary — same pattern as
//! `crates/norn-fixtures/tests/common/mod.rs`.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// Skip-if-absent oracle guard — mirrors
/// `crates/norn-fixtures/tests/oracle_smoke.rs`. `norn` (the parity
/// oracle, ADR 0018) is installed before `cargo test` in CI, so these run
/// for real there and locally whenever `norn` is on PATH; they skip
/// cleanly when it is absent.
pub fn oracle_present() -> bool {
    Command::new("norn")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The bare oracle binary name — `Command::new("norn")` resolves it via
/// PATH regardless of any later `current_dir` call, so no canonicalization
/// is needed here the way the bin's own `--oracle` flag needs it.
pub fn oracle_path() -> PathBuf {
    PathBuf::from("norn")
}

/// The workspace root, found by walking up from this crate's manifest
/// directory (`CARGO_MANIFEST_DIR`, always `.../crates/norn-parity`) to the
/// ancestor whose `Cargo.toml` declares `[workspace]`.
pub fn workspace_root() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        let candidate = dir.join("Cargo.toml");
        if candidate.is_file() {
            if let Ok(text) = std::fs::read_to_string(&candidate) {
                if text.contains("[workspace]") {
                    return dir;
                }
            }
        }
        assert!(
            dir.pop(),
            "walked past filesystem root looking for the workspace root"
        );
    }
}

/// The rewrite binary (`norn`, the phase-0 skeleton) built in the *debug*
/// profile — `cargo test --workspace` builds every workspace member's bin
/// targets as a side effect, so this exists by the time any test runs
/// without requiring a separate `cargo build --release` first. CI's
/// `cargo test --workspace --locked` step runs BEFORE its
/// `cargo build --workspace --release --locked` step, so a test that
/// required the release artifact would fail there.
pub fn rewrite_debug_binary() -> PathBuf {
    let path = workspace_root().join("target/debug/norn");
    assert!(
        path.is_file(),
        "{} not found — expected `cargo test --workspace` to have built it as a side effect",
        path.display()
    );
    path
}

/// A ledger TOML file at `path`, replacing any existing contents.
pub fn write_ledger(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}
