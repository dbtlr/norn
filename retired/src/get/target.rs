//! Wikilink-input normalization and resolution for `norn show`. Strips
//! wikilink syntax to a bare identifier and resolves it against the cache
//! via exact-path probe then case-insensitive stem scan.

/// Normalize a user-supplied target string. Accepts any of:
///   - path:        `Workspaces/norn/notes/foo.md`
///   - stem:        `foo`
///   - wikilink:    `[[foo]]`, `[[foo#anchor]]`, `[[foo^block-ref]]`, `[[foo|alias]]`
///
/// Brackets optional. Anchor / block-ref / pipe-alias suffixes are stripped
/// because they identify a position inside a doc, not which doc.
pub fn normalize_target(raw: &str) -> &str {
    let trimmed = raw.trim();
    // Strip outer [[ ]] brackets if present (paired).
    let core = if let Some(inner) = trimmed
        .strip_prefix("[[")
        .and_then(|s| s.strip_suffix("]]"))
    {
        inner.trim()
    } else {
        trimmed
    };
    // Strip from first '|' onward (pipe alias text).
    let core = core.split('|').next().unwrap_or(core);
    // Strip from first '#' or '^' onward (anchor / block-ref).
    let core = core.split(&['#', '^'][..]).next().unwrap_or(core);
    core.trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_string_passthrough() {
        assert_eq!(normalize_target("foo"), "foo");
    }

    #[test]
    fn path_passthrough() {
        assert_eq!(
            normalize_target("Workspaces/norn/notes/foo.md"),
            "Workspaces/norn/notes/foo.md"
        );
    }

    #[test]
    fn wikilink_strips_brackets() {
        assert_eq!(normalize_target("[[foo]]"), "foo");
    }

    #[test]
    fn wikilink_strips_anchor() {
        assert_eq!(normalize_target("[[foo#Some Section]]"), "foo");
    }

    #[test]
    fn wikilink_strips_block_ref() {
        assert_eq!(normalize_target("[[foo^abc-123]]"), "foo");
    }

    #[test]
    fn wikilink_strips_pipe_alias() {
        assert_eq!(normalize_target("[[foo|Display Text]]"), "foo");
    }

    #[test]
    fn wikilink_strips_anchor_and_alias() {
        assert_eq!(normalize_target("[[foo#section|Alias]]"), "foo");
    }

    #[test]
    fn no_brackets_with_anchor() {
        // Even without brackets, anchor suffix is stripped — this lets
        // agents pass the raw `link.target` string from earlier output.
        assert_eq!(normalize_target("foo#section"), "foo");
    }

    #[test]
    fn leading_trailing_whitespace_trimmed() {
        assert_eq!(normalize_target("  [[foo]]  "), "foo");
    }

    #[test]
    fn whitespace_inside_brackets_preserved_around_identifier() {
        // Inner whitespace becomes part of the identifier (paths/stems can have spaces).
        assert_eq!(normalize_target("[[Vault Memory]]"), "Vault Memory");
    }
}

use crate::cache::Cache;
use anyhow::Result;
use camino::Utf8PathBuf;

#[derive(Debug, PartialEq)]
pub struct ResolvedTarget {
    /// The raw user input (for error messages / stderr notes).
    pub raw: String,
    /// One or more resolved doc paths. Empty means "no match found."
    pub paths: Vec<Utf8PathBuf>,
}

/// Resolve a target string (path, stem, or wikilink) to one-or-more docs.
/// Unique resolution returns a single-element `paths`. Ambiguous stems
/// return all candidates. No-match returns empty `paths`; the caller
/// emits the error.
pub fn resolve_target(cache: &Cache, raw: &str) -> Result<ResolvedTarget> {
    let normalized = normalize_target(raw).to_string();
    if normalized.is_empty() {
        return Ok(ResolvedTarget {
            raw: raw.to_string(),
            paths: vec![],
        });
    }

    // 1. Exact-path probe: O(1) index lookup — avoids loading all summaries
    //    for the common case where the caller passes a full vault-relative path.
    if cache
        .document_by_path(camino::Utf8Path::new(&normalized))?
        .is_some()
    {
        return Ok(ResolvedTarget {
            raw: raw.to_string(),
            paths: vec![Utf8PathBuf::from(normalized)],
        });
    }

    // 2. Stem fallback: load all summaries once for case-insensitive stem scan.
    //    This still costs one SELECT against the documents table.
    let all = cache.documents_matching(&crate::cache::DocumentQuery::default())?;
    let stem_matches: Vec<Utf8PathBuf> = all
        .iter()
        .filter(|d| d.stem.eq_ignore_ascii_case(&normalized))
        .map(|d| d.path.clone())
        .collect();
    if !stem_matches.is_empty() {
        return Ok(ResolvedTarget {
            raw: raw.to_string(),
            paths: stem_matches,
        });
    }

    // 3. Alias fallback: only when stem found nothing AND alias_field is set.
    //    Reuses the `all` Vec from step 2 — still a single SELECT total.
    //    parse_aliases returns lowercased strings, so we compare against the
    //    lowercased target.
    if let Some(field) = cache.alias_field() {
        let target_lower = normalized.to_lowercase();
        let alias_matches: Vec<Utf8PathBuf> = all
            .iter()
            .filter(|d| {
                let (doc_aliases, _) = crate::graph::parse_aliases(d.frontmatter.as_ref(), field);
                doc_aliases.iter().any(|a| a == &target_lower)
            })
            .map(|d| d.path.clone())
            .collect();
        return Ok(ResolvedTarget {
            raw: raw.to_string(),
            paths: alias_matches,
        });
    }

    Ok(ResolvedTarget {
        raw: raw.to_string(),
        paths: vec![],
    })
}

#[cfg(test)]
mod resolver_tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    fn synth() -> (TempDir, Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-show-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(root.join("Notes.md").as_std_path(), "---\n---\n").unwrap();
        std::fs::create_dir(root.join("sub").as_std_path()).unwrap();
        std::fs::write(root.join("sub/Notes.md").as_std_path(), "---\n---\n").unwrap();
        (tmp, root)
    }

    fn open(root: &Utf8PathBuf) -> Cache {
        let mut cache = Cache::open(root).unwrap();
        cache.rebuild(root).unwrap();
        cache
    }

    #[test]
    fn exact_path_resolves_unique() {
        let (_t, root) = synth();
        let cache = open(&root);
        let r = resolve_target(&cache, "Notes.md").unwrap();
        assert_eq!(r.paths, vec![Utf8PathBuf::from("Notes.md")]);
    }

    #[test]
    fn stem_match_returns_multiple_when_ambiguous() {
        let (_t, root) = synth();
        let cache = open(&root);
        let r = resolve_target(&cache, "notes").unwrap();
        assert_eq!(r.paths.len(), 2);
        assert!(r.paths.iter().any(|p| p.as_str() == "Notes.md"));
        assert!(r.paths.iter().any(|p| p.as_str() == "sub/Notes.md"));
    }

    #[test]
    fn wikilink_normalized_then_resolved() {
        let (_t, root) = synth();
        let cache = open(&root);
        let r = resolve_target(&cache, "[[Notes]]").unwrap();
        assert_eq!(r.paths.len(), 2);
    }

    #[test]
    fn wikilink_with_anchor_resolves_doc() {
        let (_t, root) = synth();
        let cache = open(&root);
        let r = resolve_target(&cache, "[[Notes#Section]]").unwrap();
        assert_eq!(r.paths.len(), 2);
    }

    #[test]
    fn no_match_returns_empty_paths() {
        let (_t, root) = synth();
        let cache = open(&root);
        let r = resolve_target(&cache, "Nonexistent").unwrap();
        assert!(r.paths.is_empty());
    }

    #[test]
    fn alias_addressing_resolves_when_stem_returns_nothing() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-show-alias-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        // Doc with stem "vm" and alias "vault memory"
        std::fs::write(
            root.join("vm.md").as_std_path(),
            "---\naliases:\n  - Vault Memory\n---\n# Vault Memory\n",
        )
        .unwrap();

        let mut cache = crate::cache::Cache::open_with_config(&root, Some("aliases")).unwrap();
        cache.rebuild(&root).unwrap();

        // Target via wikilink shape — should resolve via alias since stem "vault memory" doesn't exist.
        let resolved = resolve_target(&cache, "[[Vault Memory]]").unwrap();
        assert_eq!(
            resolved.paths,
            vec![Utf8PathBuf::from("vm.md")],
            "expected alias resolution to find vm.md; got {:?}",
            resolved.paths
        );
    }

    #[test]
    fn alias_addressing_skipped_when_alias_field_unconfigured() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-show-no-alias-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(
            root.join("vm.md").as_std_path(),
            "---\naliases:\n  - Vault Memory\n---\n# Vault Memory\n",
        )
        .unwrap();

        // Open WITHOUT alias_field
        let mut cache = crate::cache::Cache::open_with_config(&root, None).unwrap();
        cache.rebuild(&root).unwrap();

        let resolved = resolve_target(&cache, "[[Vault Memory]]").unwrap();
        assert!(
            resolved.paths.is_empty(),
            "expected no resolution when alias_field is None; got {:?}",
            resolved.paths
        );
    }
}
