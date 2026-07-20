//! Post-state comparison for MUTATING cases (ADR 0018). After both sides run
//! a case that writes to the vault, the two resulting vault TREES are compared
//! — the set of relative paths plus each file's bytes. A difference feeds the
//! same three-verdict machinery as a stdout/stderr/exit difference (match /
//! diverged-with-ledger-entry / drift — no fourth state); the diff below is
//! purely the reporting detail of WHY a mutating case did not match.
//!
//! Scope of the walk: the vault directory tree, every regular file, addressed
//! by its forward-slashed relative path. `norn` keeps its cache and state OUT
//! of the vault (verified against the pinned oracle v0.48.1: running a command
//! with the vault as cwd leaves the tree untouched but for the command's own
//! writes; the cache lives under the user cache dir, keyed by content hash),
//! so the tree is pure logical vault content and nothing is excluded. Were a
//! future norn to drop side-specific cache artifacts inside the vault, this is
//! the one place an exclusion filter would be added.
//!
//! Normalization mirrors `crate::normalize`: each file's bytes have the side's
//! own absolute vault-root spellings replaced with `<VAULT>` before the
//! compare, so a path a mutation happens to write into a document (each side
//! under its own temp root) never registers as a divergence. Applied at the
//! byte level — vault files are text today, but a byte replace cannot corrupt
//! a non-UTF-8 file and needs no lossy decode.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

/// A vault tree snapshot: forward-slashed relative path -> file bytes, ordered
/// (a `BTreeMap`) so comparison and reporting are deterministic.
pub type Snapshot = BTreeMap<String, Vec<u8>>;

/// Walk `vault` and snapshot every regular file under it, keyed by its
/// relative path (forward slashes on every platform). Symlinks are not
/// followed. A missing file races nothing here — the tree is quiescent once
/// both binaries have exited.
pub fn snapshot(vault: &Path) -> io::Result<Snapshot> {
    let mut out = Snapshot::new();
    walk(vault, vault, &mut out)?;
    Ok(out)
}

fn walk(root: &Path, dir: &Path, out: &mut Snapshot) -> io::Result<()> {
    // Collect entries into a sorted vector first so the walk order is
    // deterministic regardless of the platform's readdir order.
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            walk(root, &path, out)?;
        } else if file_type.is_file() {
            let rel = relative_key(root, &path);
            let bytes = std::fs::read(&path)?;
            out.insert(rel, bytes);
        }
        // Symlinks / other node types: skipped — no fixture produces them.
    }
    Ok(())
}

/// The path of `file` relative to `root`, forward-slashed so keys are
/// platform-stable. `file` is always a descendant of `root` (the walk starts
/// there), so the `strip_prefix` cannot fail; the fallback keeps the full path
/// rather than panicking if that ever changes.
fn relative_key(root: &Path, file: &Path) -> String {
    let rel = file.strip_prefix(root).unwrap_or(file);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Replace every occurrence of any `vault_roots` spelling (as raw bytes) with
/// `<VAULT>` — the byte-level analogue of `crate::normalize`'s `VaultRoot`
/// step. Longer spellings are replaced first so a shorter alias never
/// partially rewrites a longer one (matching `normalize::normalize_text`).
fn normalize_bytes(bytes: &[u8], vault_roots: &[&Path]) -> Vec<u8> {
    let mut roots: Vec<Vec<u8>> = vault_roots
        .iter()
        .map(|p| p.to_string_lossy().into_owned().into_bytes())
        .filter(|b| !b.is_empty())
        .collect();
    roots.sort_by_key(|b| std::cmp::Reverse(b.len()));
    let mut out = bytes.to_vec();
    for root in &roots {
        out = replace_bytes(&out, root, b"<VAULT>");
    }
    out
}

/// Non-overlapping byte-substring replacement of `needle` with `replacement`.
fn replace_bytes(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return haystack.to_vec();
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i..].starts_with(needle) {
            out.extend_from_slice(replacement);
            i += needle.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

/// How two post-mutation vault trees differ. Empty on every axis == equal
/// (see [`PostStateDiff::is_empty`]). Reported, never a fourth verdict.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PostStateDiff {
    /// Relative paths present only on the oracle side.
    pub only_in_oracle: Vec<String>,
    /// Relative paths present only on the candidate side.
    pub only_in_candidate: Vec<String>,
    /// Relative paths present on both sides whose (normalized) bytes differ —
    /// each with a concise byte-length summary, not a full dump.
    pub content_differs: Vec<ContentDelta>,
}

/// A concise summary of one path whose content differs between sides — the
/// normalized byte lengths, enough to make the report legible without dumping
/// file bodies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentDelta {
    pub path: String,
    pub oracle_len: usize,
    pub candidate_len: usize,
}

impl PostStateDiff {
    pub fn is_empty(&self) -> bool {
        self.only_in_oracle.is_empty()
            && self.only_in_candidate.is_empty()
            && self.content_differs.is_empty()
    }
}

/// Compare the oracle and candidate post-mutation trees under vault-root
/// normalization (each side stripped by its OWN spellings). Returns `None`
/// when the trees are equal, `Some(diff)` when they differ — the caller folds
/// that into the same match/diverged/drift decision as an output difference.
pub fn compare(
    oracle: &Snapshot,
    oracle_roots: &[&Path],
    candidate: &Snapshot,
    candidate_roots: &[&Path],
) -> Option<PostStateDiff> {
    let mut diff = PostStateDiff::default();

    for (path, bytes) in oracle {
        match candidate.get(path) {
            None => diff.only_in_oracle.push(path.clone()),
            Some(cand_bytes) => {
                let o = normalize_bytes(bytes, oracle_roots);
                let c = normalize_bytes(cand_bytes, candidate_roots);
                if o != c {
                    diff.content_differs.push(ContentDelta {
                        path: path.clone(),
                        oracle_len: o.len(),
                        candidate_len: c.len(),
                    });
                }
            }
        }
    }
    for path in candidate.keys() {
        if !oracle.contains_key(path) {
            diff.only_in_candidate.push(path.clone());
        }
    }

    if diff.is_empty() {
        None
    } else {
        Some(diff)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn snap(pairs: &[(&str, &[u8])]) -> Snapshot {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_vec()))
            .collect()
    }

    #[test]
    fn identical_trees_compare_equal() {
        let a = snap(&[("a.md", b"hello"), ("sub/b.md", b"world")]);
        let b = snap(&[("a.md", b"hello"), ("sub/b.md", b"world")]);
        assert_eq!(compare(&a, &[], &b, &[]), None);
    }

    #[test]
    fn added_and_removed_paths_are_reported_per_side() {
        let oracle = snap(&[("a.md", b"x"), ("only-oracle.md", b"o")]);
        let candidate = snap(&[("a.md", b"x"), ("only-cand.md", b"c")]);
        let diff = compare(&oracle, &[], &candidate, &[]).expect("trees differ");
        assert_eq!(diff.only_in_oracle, vec!["only-oracle.md".to_string()]);
        assert_eq!(diff.only_in_candidate, vec!["only-cand.md".to_string()]);
        assert!(diff.content_differs.is_empty());
    }

    #[test]
    fn differing_bytes_at_a_shared_path_are_reported() {
        let oracle = snap(&[("a.md", b"short")]);
        let candidate = snap(&[("a.md", b"a much longer body")]);
        let diff = compare(&oracle, &[], &candidate, &[]).expect("content differs");
        assert_eq!(diff.content_differs.len(), 1);
        let delta = &diff.content_differs[0];
        assert_eq!(delta.path, "a.md");
        assert_eq!(delta.oracle_len, 5);
        assert_eq!(delta.candidate_len, 18);
    }

    #[test]
    fn side_specific_absolute_paths_are_normalized_away() {
        // Each side wrote its own temp-root path into the file; after
        // normalization both read `<VAULT>/...` and the trees are equal.
        let oracle_root = PathBuf::from("/tmp/oracle-abc/vault");
        let candidate_root = PathBuf::from("/tmp/candidate-xyz/vault");
        let oracle = snap(&[("a.md", b"see /tmp/oracle-abc/vault/a.md")]);
        let candidate = snap(&[("a.md", b"see /tmp/candidate-xyz/vault/a.md")]);
        assert_eq!(
            compare(
                &oracle,
                &[oracle_root.as_path()],
                &candidate,
                &[candidate_root.as_path()]
            ),
            None,
            "identical content modulo each side's own vault root must match"
        );
    }

    #[test]
    fn longer_root_spelling_wins_over_shorter_alias() {
        // The macOS /var vs /private/var case: the longer spelling must be
        // replaced first so the shorter alias never partially rewrites it.
        let canonical = PathBuf::from("/private/var/f/vault");
        let alias = PathBuf::from("/var/f/vault");
        let normalized = normalize_bytes(
            b"/private/var/f/vault/a.md",
            &[alias.as_path(), canonical.as_path()],
        );
        assert_eq!(normalized, b"<VAULT>/a.md".to_vec());
    }

    #[test]
    fn replace_bytes_handles_overlap_free_repeats() {
        assert_eq!(replace_bytes(b"a.a.a", b"a", b"X"), b"X.X.X".to_vec());
        assert_eq!(replace_bytes(b"abcabc", b"abc", b"Z"), b"ZZ".to_vec());
    }
}
