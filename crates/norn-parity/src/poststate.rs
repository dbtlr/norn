//! Post-state comparison for MUTATING cases (ADR 0018). After both sides run
//! a case that writes to the vault, the two resulting vault TREES are compared
//! — the set of relative paths (files, with their bytes; plus empty
//! directories) — and a difference feeds the same three-verdict machinery as a
//! stdout/stderr/exit difference (match / diverged-with-ledger-entry / drift —
//! no fourth state); the diff below is purely the reporting detail of WHY a
//! mutating case did not match.
//!
//! Scope of the walk: the vault directory tree. Every regular file is recorded
//! by its forward-slashed relative path with its bytes; every EMPTY directory
//! is recorded by a marker (non-empty directories are implied by the entries
//! beneath them). Empty dirs are tracked because directory cleanup is a real
//! verb behavior — a `move`/`delete` that removes a now-empty dir on one side
//! but leaves it on the other is a divergence a file-only walk would miss.
//!
//! Comparisons are scoped to logical IN-VAULT content by design: a write
//! OUTSIDE the vault root is invisible to post-state comparison (verified
//! fail-safe for the current verbs, which write only inside the vault), and
//! this walk is the single exclusion-filter point where any future scoping
//! (e.g. carving out an in-vault cache dir) would land. `norn` keeps its cache
//! and state OUT of the vault (verified against the pinned oracle v0.48.1:
//! running a command with the vault as cwd leaves the tree untouched but for
//! the command's own writes; the cache lives under the user cache dir, keyed by
//! content hash), so the tree is pure logical vault content and nothing is
//! excluded today. Symlinks and node metadata (mode / mtime) are likewise out
//! of scope — the comparison is over path set + file bytes + empty-dir presence
//! only.
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

/// One tree node the snapshot records: a regular file's bytes, or an empty
/// directory's presence. Non-empty directories are not recorded — they are
/// implied by the entries beneath them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    File(Vec<u8>),
    EmptyDir,
}

/// A vault tree snapshot: forward-slashed relative path -> node, ordered (a
/// `BTreeMap`) so comparison and reporting are deterministic.
pub type Snapshot = BTreeMap<String, Entry>;

/// Walk `vault` and snapshot every regular file (with its bytes) and every
/// empty directory (as a marker) under it, keyed by relative path (forward
/// slashes on every platform). Symlinks are not followed. The tree is
/// quiescent once both binaries have exited, so nothing races the walk.
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

    // An empty subdirectory (never the vault root itself) is recorded by a
    // marker so a one-sided empty dir surfaces as a divergence. A trailing `/`
    // keeps a dir key distinct from a file of the same name and reads
    // naturally in the report. The root's emptiness needs no marker: two empty
    // vaults simply share an empty snapshot and compare equal.
    if entries.is_empty() {
        if dir != root {
            out.insert(format!("{}/", relative_key(root, dir)), Entry::EmptyDir);
        }
        return Ok(());
    }

    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            walk(root, &path, out)?;
        } else if file_type.is_file() {
            let rel = relative_key(root, &path);
            let bytes = std::fs::read(&path)?;
            out.insert(rel, Entry::File(bytes));
        }
        // Symlinks / other node types: skipped — no fixture produces them.
    }
    Ok(())
}

/// The path of `node` relative to `root`, forward-slashed so keys are
/// platform-stable. `node` is always a descendant of `root` (the walk starts
/// there), so the `strip_prefix` cannot fail; the fallback keeps the full path
/// rather than panicking if that ever changes.
fn relative_key(root: &Path, node: &Path) -> String {
    let rel = node.strip_prefix(root).unwrap_or(node);
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

/// The index of the first byte at which `a` and `b` differ. When one is a
/// prefix of the other, that is the shorter length (the point the two run out
/// of common bytes). Only meaningful for inputs already known to differ.
fn first_difference(a: &[u8], b: &[u8]) -> usize {
    a.iter()
        .zip(b.iter())
        .position(|(x, y)| x != y)
        .unwrap_or_else(|| a.len().min(b.len()))
}

/// How two post-mutation vault trees differ. Empty on every axis == equal
/// (see [`PostStateDiff::is_empty`]). Reported, never a fourth verdict.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PostStateDiff {
    /// Relative paths present only on the oracle side.
    pub only_in_oracle: Vec<String>,
    /// Relative paths present only on the candidate side.
    pub only_in_candidate: Vec<String>,
    /// Relative paths present on both sides whose (normalized) content differs
    /// — each with a concise summary, not a full dump.
    pub content_differs: Vec<ContentDelta>,
}

/// A concise summary of one path whose content differs between sides — the
/// normalized byte lengths and the offset of the first differing byte, enough
/// to make the report legible (and to disambiguate same-length differences)
/// without dumping file bodies. A file-vs-empty-directory kind mismatch at one
/// path is reported here too, the directory side counted as zero bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentDelta {
    pub path: String,
    pub oracle_len: usize,
    pub candidate_len: usize,
    pub first_diff_at: usize,
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

    for (path, oracle_entry) in oracle {
        match candidate.get(path) {
            None => diff.only_in_oracle.push(path.clone()),
            Some(cand_entry) => {
                if let Some(delta) = entry_delta(
                    path,
                    oracle_entry,
                    oracle_roots,
                    cand_entry,
                    candidate_roots,
                ) {
                    diff.content_differs.push(delta);
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

/// The content delta at a path present on both sides, or `None` when the two
/// nodes are equivalent. Two empty dirs match; two files match iff their
/// normalized bytes are equal; a file-vs-empty-dir kind mismatch is always a
/// difference (the dir side counted as zero bytes).
///
/// The kind-mismatch arms are defensive: `walk` keys empty directories with a
/// trailing slash and files without, so a real file-vs-dir divergence reaches
/// `compare` as two distinct keys and surfaces as paired `only_in_*` entries —
/// the more informative report shape. The arms only fire on a hand-built
/// snapshot whose keys ignore that scheme.
fn entry_delta(
    path: &str,
    oracle_entry: &Entry,
    oracle_roots: &[&Path],
    cand_entry: &Entry,
    candidate_roots: &[&Path],
) -> Option<ContentDelta> {
    match (oracle_entry, cand_entry) {
        (Entry::EmptyDir, Entry::EmptyDir) => None,
        (Entry::File(o_raw), Entry::File(c_raw)) => {
            let o = normalize_bytes(o_raw, oracle_roots);
            let c = normalize_bytes(c_raw, candidate_roots);
            if o == c {
                None
            } else {
                Some(ContentDelta {
                    path: path.to_string(),
                    oracle_len: o.len(),
                    candidate_len: c.len(),
                    first_diff_at: first_difference(&o, &c),
                })
            }
        }
        // A path that is a file on one side and an empty directory on the other
        // — a real divergence; the directory side has no bytes.
        (Entry::File(o_raw), Entry::EmptyDir) => {
            let o = normalize_bytes(o_raw, oracle_roots);
            Some(ContentDelta {
                path: path.to_string(),
                oracle_len: o.len(),
                candidate_len: 0,
                first_diff_at: 0,
            })
        }
        (Entry::EmptyDir, Entry::File(c_raw)) => {
            let c = normalize_bytes(c_raw, candidate_roots);
            Some(ContentDelta {
                path: path.to_string(),
                oracle_len: 0,
                candidate_len: c.len(),
                first_diff_at: 0,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn snap(pairs: &[(&str, Entry)]) -> Snapshot {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn file(bytes: &[u8]) -> Entry {
        Entry::File(bytes.to_vec())
    }

    #[test]
    fn identical_trees_compare_equal() {
        let a = snap(&[("a.md", file(b"hello")), ("sub/b.md", file(b"world"))]);
        let b = snap(&[("a.md", file(b"hello")), ("sub/b.md", file(b"world"))]);
        assert_eq!(compare(&a, &[], &b, &[]), None);
    }

    #[test]
    fn added_and_removed_paths_are_reported_per_side() {
        let oracle = snap(&[("a.md", file(b"x")), ("only-oracle.md", file(b"o"))]);
        let candidate = snap(&[("a.md", file(b"x")), ("only-cand.md", file(b"c"))]);
        let diff = compare(&oracle, &[], &candidate, &[]).expect("trees differ");
        assert_eq!(diff.only_in_oracle, vec!["only-oracle.md".to_string()]);
        assert_eq!(diff.only_in_candidate, vec!["only-cand.md".to_string()]);
        assert!(diff.content_differs.is_empty());
    }

    #[test]
    fn differing_bytes_at_a_shared_path_are_reported_with_offset() {
        let oracle = snap(&[("a.md", file(b"short"))]);
        let candidate = snap(&[("a.md", file(b"a much longer body"))]);
        let diff = compare(&oracle, &[], &candidate, &[]).expect("content differs");
        assert_eq!(diff.content_differs.len(), 1);
        let delta = &diff.content_differs[0];
        assert_eq!(delta.path, "a.md");
        assert_eq!(delta.oracle_len, 5);
        assert_eq!(delta.candidate_len, 18);
        assert_eq!(delta.first_diff_at, 0, "they differ at the very first byte");
    }

    #[test]
    fn same_length_difference_is_disambiguated_by_first_diff_offset() {
        // Both bodies are 8 bytes; the byte lengths alone say nothing — the
        // first-diff offset is what makes the divergence legible.
        let oracle = snap(&[("a.md", file(b"abcXefgh"))]);
        let candidate = snap(&[("a.md", file(b"abcYefgh"))]);
        let diff = compare(&oracle, &[], &candidate, &[]).expect("content differs");
        let delta = &diff.content_differs[0];
        assert_eq!(delta.oracle_len, 8);
        assert_eq!(delta.candidate_len, 8);
        assert_eq!(delta.first_diff_at, 3);
    }

    #[test]
    fn a_one_sided_empty_directory_is_a_divergence() {
        // The delete/move directory-cleanup case: oracle removed a now-empty
        // dir, candidate left it (or vice versa). A file-only walk would call
        // this Match; the empty-dir marker makes it a divergence.
        let oracle = snap(&[("a.md", file(b"x"))]);
        let candidate = snap(&[("a.md", file(b"x")), ("emptied/", Entry::EmptyDir)]);
        let diff = compare(&oracle, &[], &candidate, &[]).expect("empty dir differs");
        assert_eq!(diff.only_in_candidate, vec!["emptied/".to_string()]);
        assert!(diff.only_in_oracle.is_empty());
        assert!(diff.content_differs.is_empty());
    }

    #[test]
    fn matching_empty_directories_compare_equal() {
        let oracle = snap(&[("keep/", Entry::EmptyDir)]);
        let candidate = snap(&[("keep/", Entry::EmptyDir)]);
        assert_eq!(compare(&oracle, &[], &candidate, &[]), None);
    }

    // Through the real walk key scheme (`x` file vs `x/` empty dir), a kind
    // mismatch at one logical path surfaces as paired only_in_* entries — a
    // divergence, never a silent match.
    #[test]
    fn file_versus_empty_dir_at_one_path_is_a_divergence() {
        let oracle = snap(&[("x", file(b"body"))]);
        let candidate = snap(&[("x/", Entry::EmptyDir)]);
        let diff = compare(&oracle, &[], &candidate, &[]).expect("kind mismatch differs");
        assert_eq!(diff.only_in_oracle, vec!["x".to_string()]);
        assert_eq!(diff.only_in_candidate, vec!["x/".to_string()]);
        assert!(diff.content_differs.is_empty());
    }

    // Defensive arms: a hand-built snapshot that ignores the walk key scheme
    // still reports a kind mismatch as a content divergence, not a match.
    #[test]
    fn identical_key_kind_mismatch_is_still_a_content_divergence() {
        let oracle = snap(&[("x", file(b"body"))]);
        let candidate = snap(&[("x", Entry::EmptyDir)]);
        let diff = compare(&oracle, &[], &candidate, &[]).expect("kind mismatch differs");
        assert_eq!(diff.content_differs.len(), 1);
        assert_eq!(diff.content_differs[0].oracle_len, 4);
        assert_eq!(diff.content_differs[0].candidate_len, 0);
    }

    #[test]
    fn side_specific_absolute_paths_are_normalized_away() {
        // Each side wrote its own temp-root path into the file; after
        // normalization both read `<VAULT>/...` and the trees are equal.
        let oracle_root = PathBuf::from("/tmp/oracle-abc/vault");
        let candidate_root = PathBuf::from("/tmp/candidate-xyz/vault");
        let oracle = snap(&[("a.md", file(b"see /tmp/oracle-abc/vault/a.md"))]);
        let candidate = snap(&[("a.md", file(b"see /tmp/candidate-xyz/vault/a.md"))]);
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

    #[test]
    fn snapshot_of_a_missing_path_is_an_io_error() {
        // The failure the runner maps to `RunError::PostStateSnapshot` (exit 2,
        // never a verdict): an unreadable/absent vault tree.
        let missing = PathBuf::from("/nonexistent-parity-vault-xyz/does/not/exist");
        assert!(
            snapshot(&missing).is_err(),
            "snapshotting a missing tree must surface an IO error, not an empty snapshot"
        );
    }
}
