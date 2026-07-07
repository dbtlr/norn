//! Pre-flight checks for `norn new`.
//!
//! Verifies the destination path is valid (`.md` extension, under vault root,
//! not a dotfile), the destination doesn't exist (unless `--force`), and the
//! parent directory exists (unless `-p` / `--parents`).

use camino::Utf8Path;

use crate::standards::apply::{ensure_within_vault, ContainmentError};

#[derive(Debug, thiserror::Error)]
pub enum PreflightError {
    #[error("path must end in .md: {0}")]
    NotMarkdown(String),
    #[error(transparent)]
    OutsideVault(#[from] ContainmentError),
    #[error("dotfile paths are excluded from vaults: {0}")]
    Dotfile(String),
    #[error("destination already exists (use --force to overwrite): {0}")]
    DestinationExists(String),
    #[error("parent directory does not exist (use -p / --parents to auto-create): {0}")]
    ParentMissing(String),
}

pub fn preflight(
    vault_root: &str,
    relative_path: &str,
    force: bool,
    parents: bool,
) -> Result<(), PreflightError> {
    if !relative_path.ends_with(".md") {
        return Err(PreflightError::NotMarkdown(relative_path.into()));
    }
    // NRN-145: reject absolute paths, `..` traversal, AND directories symlinked
    // out of the vault via the shared containment gate — one implementation with
    // the mutation stack, no parallel lexical logic. `norn new` creates one
    // document, so canonicalizing the vault root here (off any read path) is
    // cheap.
    let canonical_root = Utf8Path::new(vault_root)
        .as_std_path()
        .canonicalize()
        .map_err(|e| ContainmentError::Unresolvable {
            target: Utf8Path::new(relative_path).to_owned(),
            detail: e.to_string(),
        })?;
    ensure_within_vault(
        Utf8Path::new(vault_root),
        &canonical_root,
        Utf8Path::new(relative_path),
    )?;
    // Dotfile = any segment beginning with `.`.
    if relative_path.split('/').any(|seg| seg.starts_with('.')) {
        return Err(PreflightError::Dotfile(relative_path.into()));
    }
    let full = Utf8Path::new(vault_root).join(relative_path);
    if full.exists() && !force {
        return Err(PreflightError::DestinationExists(relative_path.into()));
    }
    if let Some(parent) = full.parent() {
        if !parent.exists() && !parents {
            return Err(PreflightError::ParentMissing(relative_path.into()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::Builder;

    fn vault() -> tempfile::TempDir {
        // macOS default temp prefix `.tmp` would be treated as hidden by the
        // vault walker. Use a non-`.`-prefixed prefix.
        Builder::new()
            .prefix("vault-new-validate-")
            .tempdir()
            .unwrap()
    }

    #[test]
    fn rejects_non_md_extension() {
        let root = vault();
        let err =
            preflight(root.path().to_str().unwrap(), "notes/foo.txt", false, false).unwrap_err();
        assert!(
            err.to_string().contains(".md") || err.to_string().to_lowercase().contains("markdown"),
            "expected .md error, got: {err}"
        );
    }

    #[test]
    fn rejects_absolute_path() {
        let root = vault();
        let err = preflight(
            root.path().to_str().unwrap(),
            "/absolute/path.md",
            false,
            false,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("vault root") || err.to_string().contains("absolute"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_parent_escape() {
        let root = vault();
        let err =
            preflight(root.path().to_str().unwrap(), "../escape.md", false, false).unwrap_err();
        assert!(
            err.to_string().contains("vault root") || err.to_string().contains("escape"),
            "got: {err}"
        );
    }

    /// NRN-145: a symlinked directory inside the vault pointing outside bypasses
    /// the lexical check (no `..`, not absolute), so the shared containment gate
    /// must refuse it here too — the vault is self-contained.
    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_dir_escape() {
        let root = vault();
        let outside = Builder::new()
            .prefix("vault-new-validate-target-")
            .tempdir()
            .unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("outlink")).unwrap();
        let err = preflight(
            root.path().to_str().unwrap(),
            "outlink/escape.md",
            false,
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("vault"), "got: {err}");
    }

    #[test]
    fn rejects_dotfile() {
        let root = vault();
        let err = preflight(root.path().to_str().unwrap(), ".hidden.md", false, false).unwrap_err();
        assert!(
            err.to_string().contains("dotfile") || err.to_string().contains("hidden"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_existing_path_without_force() {
        let root = vault();
        std::fs::write(root.path().join("foo.md"), "existing").unwrap();
        let err = preflight(root.path().to_str().unwrap(), "foo.md", false, false).unwrap_err();
        assert!(err.to_string().contains("exists"), "got: {err}");
    }

    #[test]
    fn accepts_existing_path_with_force() {
        let root = vault();
        std::fs::write(root.path().join("foo.md"), "existing").unwrap();
        preflight(root.path().to_str().unwrap(), "foo.md", true, false).unwrap();
    }

    #[test]
    fn rejects_missing_parent_without_parents() {
        let root = vault();
        let err = preflight(
            root.path().to_str().unwrap(),
            "deep/nested/dir/foo.md",
            false,
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("parent"), "got: {err}");
    }

    #[test]
    fn accepts_missing_parent_with_parents_flag() {
        let root = vault();
        preflight(
            root.path().to_str().unwrap(),
            "deep/nested/dir/foo.md",
            false,
            true,
        )
        .unwrap();
    }

    #[test]
    fn accepts_existing_parent() {
        let root = vault();
        std::fs::create_dir_all(root.path().join("notes")).unwrap();
        preflight(root.path().to_str().unwrap(), "notes/foo.md", false, false).unwrap();
    }
}
