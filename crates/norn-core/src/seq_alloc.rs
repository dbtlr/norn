//! Apply-time `{{seq}}` resolution (NRN-101): the next incremental id for a
//! rule-targeted create, computed as filesystem **max+1** over sibling files
//! that share the template's non-seq prefix/suffix.
//!
//! # Coupling to the single-writer boundary (load-bearing)
//!
//! `max+1` allocation is only correct when it runs inside the same critical
//! section that serializes every writer to one vault, so two concurrent creates
//! observe each other's files and get distinct sequential ids. In the pre-owner
//! world that boundary was a cross-process advisory `flock` (the mutation lock);
//! under the summoned-owner model (ADR 0013/0017) it is a triad: the owner-
//! lifetime `flock` (`acquire_owner_lock`, flock-then-bind) guarantees one
//! owner process per vault, the client's connect-or-summon path leaves no
//! direct-write route, and the owner's in-process single-writer queue then
//! serializes writes within that process. Either way the invariant is the same and
//! it is NOT enforced by this module: the caller MUST hold the writer boundary
//! across [`resolve_seq_create`] and the subsequent create. Resolving `{{seq}}`
//! outside that boundary races and can mint duplicate ids.

use camino::{Utf8Path, Utf8PathBuf};

/// The rule-target placeholder resolved to the next sequential id.
pub const SEQ_TOKEN: &str = "{{seq}}";

/// Does this path carry an unresolved `{{seq}}` token?
pub fn has_seq(path: &Utf8Path) -> bool {
    path.as_str().contains(SEQ_TOKEN)
}

/// File names (not full paths) of the entries directly in `dir`.
///
/// A **missing** directory yields an empty list — a create into a not-yet-existing
/// folder legitimately starts its `{{seq}}` sequence at 1. Any *other* read error
/// (permissions, EMFILE, …) is propagated, never coerced to empty: coercing would
/// silently reset the sequence to 1 and — under `--force` — overwrite the real
/// highest-id file.
pub fn dir_file_names(dir: &Utf8Path) -> std::io::Result<Vec<String>> {
    match std::fs::read_dir(dir.as_std_path()) {
        Ok(entries) => Ok(entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Resolve a `{{seq}}` `template` (vault-relative) against the live filesystem
/// rooted at `cwd`. Reads the template's parent directory and applies
/// [`resolve_seq`]. Used both at apply time (under the writer boundary,
/// authoritative) and for dry-run prediction (non-binding — a concurrent create
/// could take the id).
pub fn predict(cwd: &Utf8Path, template: &Utf8Path) -> std::io::Result<Utf8PathBuf> {
    let dir = cwd.join(template.parent().unwrap_or_else(|| Utf8Path::new("")));
    Ok(resolve_seq(template, &dir_file_names(&dir)?))
}

/// Resolve a `{{seq}}` create `template` (vault-relative) at apply time: scan
/// the template's parent directory under `base_dir`, fold in same-directory
/// paths already allocated by earlier creates in this plan (not necessarily on
/// disk yet — NRN-101), apply [`resolve_seq`], and refuse if a token survives
/// (it appeared in a directory component, or more than once). This is the ONE
/// allocation composition shared by the plan applier's pre-resolution barrier
/// and the apply delegate's create resolver, so the two sites cannot drift in
/// allocation semantics (NRN-265).
pub fn resolve_seq_create(
    base_dir: &Utf8Path,
    template: &Utf8Path,
    allocated_this_plan: &[Utf8PathBuf],
) -> anyhow::Result<Utf8PathBuf> {
    use anyhow::Context as _;
    let dir = base_dir.join(template.parent().unwrap_or_else(|| Utf8Path::new("")));
    let mut siblings = dir_file_names(&dir)
        .with_context(|| format!("create_document: scan {dir} for {{{{seq}}}}"))?;
    for prior in allocated_this_plan {
        if prior.parent() == template.parent() {
            if let Some(name) = prior.file_name() {
                siblings.push(name.to_string());
            }
        }
    }
    let resolved = resolve_seq(template, &siblings);
    // `{{seq}}` is only resolvable once, in the file name. If any token
    // survives, refuse rather than emit a path with a literal `{{seq}}` in it.
    if has_seq(&resolved) {
        anyhow::bail!(
            "create_document: `{{{{seq}}}}` is only supported once, in the file name of a rule target: {template}"
        );
    }
    Ok(resolved)
}

/// True when `path` carries a `{{seq}}` token that cannot be resolved: the token
/// must appear **exactly once, in the file name**. A token in a directory
/// component, or a second occurrence, is a rule misconfiguration. Lets callers
/// fail fast at plan/generate time rather than deferring the refusal to apply.
pub fn seq_misplaced(path: &Utf8Path) -> bool {
    let total = path.as_str().matches(SEQ_TOKEN).count();
    if total == 0 {
        return false;
    }
    let in_name = path.file_name().map_or(0, |n| n.matches(SEQ_TOKEN).count());
    total != 1 || in_name != 1
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

    #[test]
    fn seq_misplaced_flags_only_bad_placements() {
        assert!(!seq_misplaced(Utf8Path::new("tasks/MMR-{{seq}}.md"))); // one, in name → ok
        assert!(!seq_misplaced(Utf8Path::new("tasks/fixed.md"))); // no token → ok
        assert!(seq_misplaced(Utf8Path::new("tasks/{{seq}}/note.md"))); // dir component
        assert!(seq_misplaced(Utf8Path::new("tasks/MMR-{{seq}}-{{seq}}.md"))); // twice
        assert!(seq_misplaced(Utf8Path::new("{{seq}}/MMR-{{seq}}.md"))); // dir + name
    }

    #[test]
    fn dir_file_names_missing_dir_is_empty_not_error() {
        let out = dir_file_names(Utf8Path::new("/no/such/norn/dir/xyz")).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn seq_only_in_directory_component_is_left_unresolved() {
        // `{{seq}}` outside the file name is not resolved here — the applier
        // detects the surviving token and refuses rather than writing a literal.
        let out = resolve_seq(Utf8Path::new("tasks/{{seq}}/note.md"), &[]);
        assert_eq!(out, Utf8PathBuf::from("tasks/{{seq}}/note.md"));
        assert!(has_seq(&out));
    }

    #[test]
    fn resolve_seq_create_folds_in_prior_allocations() {
        // Two creates into the same dir in one plan: the second sees the first's
        // not-yet-on-disk allocation and advances past it.
        let tmp = tempfile::TempDir::new().unwrap();
        let base = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let template = Utf8Path::new("tasks/MMR-{{seq}}.md");
        let first = resolve_seq_create(&base, template, &[]).unwrap();
        assert_eq!(first, Utf8PathBuf::from("tasks/MMR-1.md"));
        let second = resolve_seq_create(&base, template, std::slice::from_ref(&first)).unwrap();
        assert_eq!(second, Utf8PathBuf::from("tasks/MMR-2.md"));
    }

    #[test]
    fn resolve_seq_create_refuses_surviving_token() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        // A token in a directory component cannot be resolved.
        let err = resolve_seq_create(&base, Utf8Path::new("{{seq}}/note.md"), &[])
            .expect_err("surviving token must refuse");
        assert!(err.to_string().contains("only supported once"));
    }
}
