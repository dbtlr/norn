//! Shared test helpers. `tests/common/mod.rs` (not `tests/common.rs`) so
//! cargo treats it as a module included by each integration test rather
//! than its own test binary — same pattern as
//! `crates/norn-fixtures/tests/common/mod.rs`.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

pub use norn_fixtures::testing::oracle_present;

/// The single skip-if-no-oracle guard for every oracle-touching test in this
/// crate: when `norn` is absent it prints one `skip:` line naming the suite
/// and returns `true`, so the caller can `return`. Collapses the seven
/// verbatim `if !oracle_present() { eprintln!(...); return; }` blocks into
/// one call site shape. Wraps the shared probe in
/// `norn_fixtures::testing::oracle_present`.
pub fn oracle_missing(suite: &str) -> bool {
    if oracle_present() {
        false
    } else {
        eprintln!("skip: `norn` not found on PATH — {suite} skipped");
        true
    }
}

/// The bare oracle binary name — `Command::new("norn")` resolves it via
/// PATH regardless of any later `current_dir` call, so no canonicalization
/// is needed here the way the bin's own `--oracle` flag needs it.
pub fn oracle_path() -> PathBuf {
    PathBuf::from("norn")
}

/// The workspace root, found by walking up from this crate's manifest
/// directory (`CARGO_MANIFEST_DIR`, always `.../crates/norn-parity`) to the
/// ancestor whose `Cargo.toml` declares `[workspace]`. Delegates to the
/// crate's one discovery impl (`norn_parity::paths`).
pub fn workspace_root() -> PathBuf {
    let start = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    norn_parity::paths::find_workspace_root(&start)
        .expect("walked past filesystem root looking for the workspace root")
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
