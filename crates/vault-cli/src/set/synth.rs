//! `vault set` plan synthesis: CLI args → RepairPlan.

use anyhow::{anyhow, bail, Result};
use camino::Utf8PathBuf;
use vault_cache::Cache;

/// Resolve the user-supplied DOC argument into a vault-relative path.
/// Accepts path, stem, or wikilink-shaped input (with or without [[]]).
/// Anchor / block-ref / pipe-alias suffixes are stripped before resolution.
///
/// Refuses (Err) when:
/// - The target doesn't resolve to any doc.
/// - The target resolves to multiple docs (ambiguous stem).
#[allow(dead_code)] // wired in when Command::Set handler lands (Task 2.2)
pub fn resolve_target(cache: &Cache, raw: &str) -> Result<Utf8PathBuf> {
    let resolved = crate::show::target::resolve_target(cache, raw)?;
    match resolved.paths.len() {
        0 => bail!("doc not found: {raw}"),
        1 => Ok(resolved.paths.into_iter().next().unwrap()),
        n => {
            let candidates: Vec<String> = resolved.paths.iter().map(|p| p.to_string()).collect();
            Err(anyhow!(
                "ambiguous doc target: '{raw}' matches {n} docs: {}",
                candidates.join(", ")
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vault_cache::Cache;

    fn fixture_cache() -> (tempfile::TempDir, Cache) {
        let tmp = tempfile::Builder::new()
            .prefix("vault-cli-set-resolve-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path())
            .unwrap()
            .to_path_buf();

        std::fs::create_dir_all(tmp.path().join(".vault")).unwrap();
        std::fs::write(tmp.path().join(".vault/config.yaml"), "validate: {}\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("notes")).unwrap();
        std::fs::write(tmp.path().join("notes/foo.md"), "---\ntype: note\n---\n").unwrap();
        std::fs::write(tmp.path().join("notes/bar.md"), "---\ntype: note\n---\n").unwrap();

        let mut cache = Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();
        (tmp, cache)
    }

    #[test]
    fn resolve_target_accepts_relative_path() {
        let (_tmp, cache) = fixture_cache();
        let path = resolve_target(&cache, "notes/foo.md").expect("path should resolve");
        assert_eq!(path.as_str(), "notes/foo.md");
    }

    #[test]
    fn resolve_target_accepts_bare_stem() {
        let (_tmp, cache) = fixture_cache();
        let path = resolve_target(&cache, "foo").expect("stem should resolve");
        assert_eq!(path.as_str(), "notes/foo.md");
    }

    #[test]
    fn resolve_target_accepts_wikilink_shape_with_brackets() {
        let (_tmp, cache) = fixture_cache();
        let path = resolve_target(&cache, "[[foo]]").expect("wikilink should resolve");
        assert_eq!(path.as_str(), "notes/foo.md");
    }

    #[test]
    fn resolve_target_strips_anchor_and_pipe_suffixes() {
        let (_tmp, cache) = fixture_cache();
        let path = resolve_target(&cache, "foo#section|alias").expect("should strip suffixes");
        assert_eq!(path.as_str(), "notes/foo.md");
    }

    #[test]
    fn resolve_target_returns_error_when_not_found() {
        let (_tmp, cache) = fixture_cache();
        let result = resolve_target(&cache, "nonexistent");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not found") || err.contains("nonexistent"));
    }

    #[test]
    fn resolve_target_returns_error_when_ambiguous() {
        let tmp = tempfile::Builder::new()
            .prefix("vault-cli-set-ambig-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path())
            .unwrap()
            .to_path_buf();
        std::fs::create_dir_all(tmp.path().join(".vault")).unwrap();
        std::fs::write(tmp.path().join(".vault/config.yaml"), "validate: {}\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("a")).unwrap();
        std::fs::create_dir_all(tmp.path().join("b")).unwrap();
        std::fs::write(tmp.path().join("a/shared.md"), "---\ntype: note\n---\n").unwrap();
        std::fs::write(tmp.path().join("b/shared.md"), "---\ntype: note\n---\n").unwrap();

        let mut cache = Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        let result = resolve_target(&cache, "shared");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("ambiguous"));
        assert!(err.contains("a/shared.md") || err.contains("b/shared.md"));
    }
}
