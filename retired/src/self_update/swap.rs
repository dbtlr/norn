//! Atomic binary swap via rename(2).

use std::fs;
use std::path::Path;

use anyhow::{anyhow, Result};

/// Replace `dest` with `new_binary` atomically (within a single filesystem).
/// Caller is responsible for placing `new_binary` on the same filesystem as
/// `dest` (see `download::sibling_temp_path`).
pub fn swap(new_binary: &Path, dest: &Path) -> Result<()> {
    fs::rename(new_binary, dest)
        .map_err(|e| anyhow!("rename {} -> {}: {e}", new_binary.display(), dest.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_replaces_dest_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("vault");
        let new = tmp.path().join("vault.new");
        fs::write(&dest, b"old").unwrap();
        fs::write(&new, b"new").unwrap();
        swap(&new, &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"new");
        assert!(!new.exists(), "new should have been moved");
    }
}
