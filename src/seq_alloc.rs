//! Apply-time `{{seq}}` resolution (NRN-101): the next incremental id for a
//! rule-targeted create, computed as filesystem **max+1** over sibling files
//! that share the template's non-seq prefix/suffix.
//!
//! This runs inside the mutation-lock critical section the apply already holds
//! (see `src/mutation_lock`), so two concurrent creates observe each other's
//! files and get distinct sequential ids. The NRN-87 warm daemon inherits the
//! same lock boundary: when it becomes the single writer, this allocation moves
//! behind its in-process serialization untouched — no new lock primitive here.

use camino::{Utf8Path, Utf8PathBuf};

use crate::new::generate::SEQ_TOKEN;

/// Does this path carry an unresolved `{{seq}}` token?
pub fn has_seq(path: &Utf8Path) -> bool {
    path.as_str().contains(SEQ_TOKEN)
}

/// File names (not full paths) of the entries directly in `dir`. Empty when the
/// directory is missing or unreadable — a create into a not-yet-existing folder
/// then correctly starts its `{{seq}}` sequence at 1.
pub fn dir_file_names(dir: &Utf8Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir.as_std_path()) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect()
}

/// Resolve a `{{seq}}` `template` (vault-relative) against the live filesystem
/// rooted at `cwd`. Reads the template's parent directory and applies
/// [`resolve_seq`]. Used both at apply time (under the lock, authoritative) and
/// for dry-run prediction (non-binding — a concurrent create could take the id).
pub fn predict(cwd: &Utf8Path, template: &Utf8Path) -> Utf8PathBuf {
    let dir = cwd.join(template.parent().unwrap_or_else(|| Utf8Path::new("")));
    resolve_seq(template, &dir_file_names(&dir))
}

/// Resolve `{{seq}}` in `template`'s file name to the next id, given the file
/// names present in the template's parent directory (`siblings`).
///
/// The next id is `max(existing ids) + 1`, or `1` when no sibling matches —
/// where a sibling matches iff its name is exactly `<prefix><digits><suffix>`
/// for the template's file-name prefix/suffix around `{{seq}}`. This scopes the
/// counter to the fully-bound prefix: `MMR-{{seq}}.md` counts only `MMR-*.md`,
/// independent of `NRN-*.md`.
///
/// Returns the template unchanged when its file name carries no `{{seq}}`.
pub fn resolve_seq(template: &Utf8Path, siblings: &[String]) -> Utf8PathBuf {
    let name = match template.file_name() {
        Some(n) if n.contains(SEQ_TOKEN) => n,
        _ => return template.to_path_buf(),
    };
    // `split_once` on the first token; a second `{{seq}}` (misconfig) lands in
    // `suffix` and simply never matches a sibling, so the counter stays at 1.
    let (prefix, suffix) = name.split_once(SEQ_TOKEN).expect("name contains SEQ_TOKEN");
    let next = siblings
        .iter()
        .filter_map(|s| seq_of(s, prefix, suffix))
        .max()
        .map_or(1, |m| m + 1);
    let resolved_name = format!("{prefix}{next}{suffix}");
    match template.parent() {
        Some(dir) => dir.join(resolved_name),
        None => Utf8PathBuf::from(resolved_name),
    }
}

/// Extract the integer id from `name` iff it is exactly
/// `<prefix><non-empty-digits><suffix>`.
fn seq_of(name: &str, prefix: &str, suffix: &str) -> Option<u64> {
    let middle = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
    if middle.is_empty() || !middle.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    middle.parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_prefix_starts_at_one() {
        let out = resolve_seq(Utf8Path::new("tasks/task-{{seq}}.md"), &[]);
        assert_eq!(out, Utf8PathBuf::from("tasks/task-1.md"));
    }

    #[test]
    fn max_plus_one_over_existing() {
        let siblings = vec![
            "task-1.md".to_string(),
            "task-7.md".to_string(),
            "task-3.md".to_string(),
        ];
        let out = resolve_seq(Utf8Path::new("tasks/task-{{seq}}.md"), &siblings);
        assert_eq!(out, Utf8PathBuf::from("tasks/task-8.md"));
    }

    #[test]
    fn per_prefix_isolation() {
        // NRN-* files must not advance the MMR counter.
        let siblings = vec![
            "MMR-4.md".to_string(),
            "NRN-99.md".to_string(),
            "MMR-5.md".to_string(),
        ];
        let out = resolve_seq(Utf8Path::new("tasks/MMR-{{seq}}.md"), &siblings);
        assert_eq!(out, Utf8PathBuf::from("tasks/MMR-6.md"));
    }

    #[test]
    fn ignores_non_matching_names() {
        // A non-numeric middle, a foreign prefix, and the wrong suffix are all skipped.
        let siblings = vec![
            "task-.md".to_string(),
            "task-1-2.md".to_string(),
            "other-9.md".to_string(),
            "task-3.txt".to_string(),
        ];
        let out = resolve_seq(Utf8Path::new("task-{{seq}}.md"), &siblings);
        assert_eq!(out, Utf8PathBuf::from("task-1.md"));
    }

    #[test]
    fn no_seq_token_returns_unchanged() {
        let out = resolve_seq(Utf8Path::new("tasks/fixed.md"), &["fixed.md".to_string()]);
        assert_eq!(out, Utf8PathBuf::from("tasks/fixed.md"));
    }
}
