//! Shared helpers for the summon integration tests. The owner exe is the real
//! `norn` bin (which runs the owner runtime under the `__norn-owner` sentinel),
//! built once per test process via the same cargo that runs the tests.

#![allow(dead_code)]

use std::path::PathBuf;

/// Walk up to the workspace root (the ancestor whose `Cargo.toml` declares
/// `[workspace]`).
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
        assert!(dir.pop(), "walked past filesystem root looking for workspace root");
    }
}

/// The built `norn` bin, in the debug profile. `cargo test` does not reliably
/// uplift another member's bin, so build it explicitly once per test process.
pub fn norn_bin() -> PathBuf {
    static BUILD: std::sync::Once = std::sync::Once::new();
    BUILD.call_once(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = std::process::Command::new(cargo)
            .args(["build", "-p", "norn", "--bin", "norn"])
            .current_dir(workspace_root())
            .status()
            .expect("failed to spawn cargo to build the norn bin");
        assert!(status.success(), "cargo build -p norn --bin norn failed");
    });
    let path = workspace_root().join("target/debug/norn");
    assert!(path.is_file(), "{} not found after build", path.display());
    path
}

/// Create a TempDir vault with `n` trivial notes; returns the TempDir (keep it
/// alive) and the vault root path.
pub fn temp_vault(n: usize) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    for i in 0..n {
        std::fs::write(
            root.join(format!("note-{i}.md")),
            format!("---\ntype: note\ntitle: Note {i}\n---\nbody {i}\n"),
        )
        .unwrap();
    }
    (tmp, root)
}

/// Poll `cond` until true or `budget` elapses. Bounded wait on a condition —
/// not a sleep-as-synchronization.
pub fn wait_until(budget: std::time::Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < budget {
        if cond() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    cond()
}

/// Count `norn-owner-db-*` dirs directly under `runtime_dir` (the owner's
/// born-with-owner db lives in one; zero means the db was reaped).
pub fn owner_db_dirs(runtime_dir: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(runtime_dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("norn-owner-db-")
        })
        .count()
}
