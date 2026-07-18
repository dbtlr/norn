//! Workspace-root discovery — one implementation, used by the bin (to
//! resolve the default `--ledger` path) and by the integration tests (to
//! locate `docs/parity-ledger.toml` and `target/`).

use std::path::{Path, PathBuf};

/// Walk up from `start` to the nearest ancestor directory whose `Cargo.toml`
/// declares `[workspace]`. `None` if no such ancestor exists.
pub fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        let candidate = dir.join("Cargo.toml");
        if candidate.is_file() {
            if let Ok(text) = std::fs::read_to_string(&candidate) {
                if text.contains("[workspace]") {
                    return Some(dir);
                }
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}
