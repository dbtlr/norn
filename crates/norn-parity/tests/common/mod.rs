//! Shared test helpers. `tests/common/mod.rs` (not `tests/common.rs`) so
//! cargo treats it as a module included by each integration test rather
//! than its own test binary — same pattern as
//! `crates/norn-fixtures/tests/common/mod.rs`.

#![allow(dead_code)]

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
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
/// profile. `cargo test` does NOT reliably uplift another workspace
/// member's bin to `target/debug/norn` (it landed there locally only as a
/// stale artifact of earlier `cargo build` runs; a fresh CI runner has no
/// such file), so build it explicitly — once per test process — via the
/// same cargo that is running the tests.
pub fn rewrite_debug_binary() -> PathBuf {
    static BUILD: std::sync::Once = std::sync::Once::new();
    BUILD.call_once(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = std::process::Command::new(cargo)
            .args(["build", "-p", "norn", "--bin", "norn"])
            .current_dir(workspace_root())
            .status()
            .expect("failed to spawn cargo to build the rewrite skeleton binary");
        assert!(status.success(), "cargo build -p norn --bin norn failed");
    });
    let path = workspace_root().join("target/debug/norn");
    assert!(
        path.is_file(),
        "{} not found even after `cargo build -p norn --bin norn` — non-default CARGO_TARGET_DIR?",
        path.display()
    );
    path
}

/// Write `body` as an executable `/bin/sh` script at `dir/name` — the one
/// place this crate's tests materialize a fake binary, so every suite
/// driving stubs (`tests/mcp.rs`, `tests/mutation.rs`) gets the same
/// exec-safety handling.
///
/// The mode is set at open time and the handle is flushed and closed before
/// this returns: a writable descriptor still open on the image is what makes
/// `execve` refuse with `ExecutableFileBusy`, and a later `set_permissions`
/// round-trip would widen that window for nothing. The residual cross-thread
/// window (another test thread forking while this write is in flight, so its
/// child inherits the descriptor) is absorbed by `exec`'s retry.
pub fn write_stub(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o755)
        .open(&path)
        .unwrap();
    file.write_all(body.as_bytes()).unwrap();
    file.sync_all().unwrap();
    drop(file);
    path
}

/// A ledger TOML file at `path`, replacing any existing contents.
pub fn write_ledger(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}
