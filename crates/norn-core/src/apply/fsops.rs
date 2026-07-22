//! Filesystem write primitives for the mutation apply engine.
//!
//! These are the narrow, named operations that actually touch the disk — the
//! atomic content write, the move and delete of a whole document, the
//! create-document materialization, and the vault-root containment gate every
//! op target must pass before any of them run. They are deliberately kept
//! separate from the pure content transforms (`standards::apply`) and from the
//! orchestration that sequences them (`apply::passes::run_apply_passes`): a filesystem
//! effect lives here, a byte transform lives there, and the pass structure
//! decides which runs when.
//!
//! Durability (NRN-159) and no-clobber create (NRN-160) are properties of these
//! primitives, so they are enforced here once for every write path.

use std::fs;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use camino::{Utf8Path, Utf8PathBuf};

use crate::standards::apply::{ApplyError, ContainmentError, DeleteResult, MoveResult};
use crate::standards::ApplyOp;

/// (NRN-145) Refuse an op target that would resolve outside the vault root. The
/// shared containment gate for the mutation stack (create / move / delete / edit
/// / backlink-cascade targets) and `norn new`'s path validation — one
/// implementation, no parallel logic.
///
/// The check is lexical first (cheap): an absolute path or any `..` component is
/// refused up front. Then:
///
/// - If the target ALREADY EXISTS (a backlink-cascade rewrite source, a
///   move/delete source — these always exist), the target ITSELF is
///   canonicalized and confirmed prefix-contained in `canonical_root`. This is
///   the F1 fix (NRN-145 follow-up): a symlink FILE inside the vault whose
///   PARENT is legitimately in-vault but which itself resolves outside would
///   pass a parent-only check, then a bare `fs::write`/`fs::read_to_string`
///   would follow it straight through to the outside file. Canonicalizing the
///   target closes that.
/// - Otherwise (a create/move destination that does not yet exist, so there is
///   nothing to canonicalize at the target itself), the op target's PARENT
///   directory is resolved and its nearest EXISTING ancestor is canonicalized
///   and confirmed prefix-contained in `canonical_root`. Canonicalizing the
///   parent resolves a directory symlinked out of the vault — the case the
///   lexical check alone bypasses. Canonicalizing the nearest existing
///   ancestor means `-p`/`--parents` creation of a not-yet-existing subtree
///   cannot be used to sidestep the gate.
///
/// `canonical_root` is the caller's canonicalization of the vault root; it is
/// canonicalized ONCE per apply (not per op) and never on a read path.
pub fn ensure_within_vault(
    vault_root: &Utf8Path,
    canonical_root: &std::path::Path,
    target: &Utf8Path,
) -> Result<(), ContainmentError> {
    if target.is_absolute() {
        return Err(ContainmentError::AbsolutePath {
            target: target.to_owned(),
        });
    }
    if target
        .components()
        .any(|c| matches!(c, camino::Utf8Component::ParentDir))
    {
        return Err(ContainmentError::ParentTraversal {
            target: target.to_owned(),
        });
    }

    let joined = vault_root.join(target);

    if joined.as_std_path().exists() {
        let canonical_target =
            joined
                .as_std_path()
                .canonicalize()
                .map_err(|e| ContainmentError::Unresolvable {
                    target: target.to_owned(),
                    detail: e.to_string(),
                })?;
        if !canonical_target.starts_with(canonical_root) {
            return Err(ContainmentError::EscapesVault {
                target: target.to_owned(),
            });
        }
        return Ok(());
    }

    let parent = joined.parent().unwrap_or(vault_root);
    let existing = nearest_existing_ancestor(parent);
    let canonical_parent =
        existing
            .as_std_path()
            .canonicalize()
            .map_err(|e| ContainmentError::Unresolvable {
                target: target.to_owned(),
                detail: e.to_string(),
            })?;
    if !canonical_parent.starts_with(canonical_root) {
        return Err(ContainmentError::EscapesVault {
            target: target.to_owned(),
        });
    }
    Ok(())
}

/// The nearest ancestor of `path` (inclusive) that exists on disk, following
/// symlinks. Terminates because the vault root — an ancestor of every
/// lexically-contained target — always exists.
fn nearest_existing_ancestor(path: &Utf8Path) -> &Utf8Path {
    let mut cur = path;
    loop {
        if cur.as_std_path().exists() {
            return cur;
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return cur,
        }
    }
}

/// Performs the filesystem move for a `move_document` ApplyOp.
/// Refuses with precondition errors if source is missing/symlink or
/// destination exists. Falls back to copy+remove if rename fails
/// (typically cross-device).
pub fn apply_move(cwd: &Utf8Path, change: &ApplyOp) -> Result<MoveResult, ApplyError> {
    let source_rel = &change.path;
    let dest_rel = change
        .destination
        .as_ref()
        .ok_or_else(|| ApplyError::UnsupportedOperation {
            path: source_rel.clone(),
            operation: "move_document missing destination".to_string(),
        })?;

    let source_abs = cwd.join(source_rel);
    let dest_abs = cwd.join(dest_rel);

    let metadata = fs::symlink_metadata(source_abs.as_std_path()).map_err(|_| {
        ApplyError::MoveSourceMissing {
            path: source_rel.clone(),
        }
    })?;
    if metadata.file_type().is_symlink() {
        return Err(ApplyError::MoveSourceIsSymlink {
            path: source_rel.clone(),
        });
    }
    if dest_abs.as_std_path().exists() {
        if change.force {
            // Best-effort atomicity: remove destination, then attempt rename.
            // If rename fails after this, destination is gone with no rollback.
            // Future improvement: snapshot-and-restore for true atomicity.
            fs::remove_file(dest_abs.as_std_path()).map_err(|e| ApplyError::CannotMinimalEdit {
                path: dest_rel.clone(),
                reason: format!("force-remove destination failed: {e}"),
            })?;
        } else {
            return Err(ApplyError::MoveDestinationExists {
                destination: dest_rel.clone(),
            });
        }
    }
    if let Some(parent) = dest_abs.parent() {
        fs::create_dir_all(parent.as_std_path()).map_err(|e| ApplyError::CannotMinimalEdit {
            path: dest_rel.clone(),
            reason: format!("create parent dir failed: {e}"),
        })?;
    }

    match fs::rename(source_abs.as_std_path(), dest_abs.as_std_path()) {
        Ok(()) => Ok(MoveResult {
            from: source_rel.clone(),
            to: dest_rel.clone(),
        }),
        Err(_) => {
            // Cross-device fallback
            fs::copy(source_abs.as_std_path(), dest_abs.as_std_path()).map_err(|e| {
                ApplyError::CannotMinimalEdit {
                    path: dest_rel.clone(),
                    reason: format!("copy failed: {e}"),
                }
            })?;
            fs::remove_file(source_abs.as_std_path()).map_err(|e| {
                ApplyError::CannotMinimalEdit {
                    path: source_rel.clone(),
                    reason: format!("remove source after copy failed: {e}"),
                }
            })?;
            Ok(MoveResult {
                from: source_rel.clone(),
                to: dest_rel.clone(),
            })
        }
    }
}

/// Performs the filesystem removal for a `delete_document` ApplyOp.
/// Refuses with precondition errors if source is missing or is a symlink.
pub fn apply_delete(cwd: &Utf8Path, change: &ApplyOp) -> Result<DeleteResult, ApplyError> {
    let source_rel = &change.path;
    let source_abs = cwd.join(source_rel);

    let metadata = fs::symlink_metadata(source_abs.as_std_path()).map_err(|_| {
        ApplyError::DeleteSourceMissing {
            path: source_rel.clone(),
        }
    })?;
    if metadata.file_type().is_symlink() {
        return Err(ApplyError::DeleteSourceIsSymlink {
            path: source_rel.clone(),
        });
    }

    fs::remove_file(source_abs.as_std_path()).map_err(|e| ApplyError::CannotMinimalEdit {
        path: source_rel.clone(),
        reason: format!("delete failed: {e}"),
    })?;

    Ok(DeleteResult {
        path: source_rel.clone(),
    })
}

/// Process-static, monotonically increasing suffix source for temp names. Never
/// resets within a process, so no two temp names collide even for the same
/// target stem written back-to-back.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// The sibling temp path (`.{stem}.{pid}-{seq}.tmp`) an atomic write stages into
/// before the rename. A dotfile beside the target so it lands on the same
/// filesystem (a cross-device rename would not be atomic) and is skipped by the
/// vault scan.
///
/// The `{pid}-{seq}` suffix makes every temp name UNIQUE (NRN-406 review):
/// a deterministic `.{stem}.tmp` could be reused by a later write for the same
/// stem, and if a prior write's best-effort temp cleanup had failed leaving that
/// name hard-linked to the LIVE inode, the reusing `File::create` would open it
/// `O_TRUNC` and truncate the live document through the shared inode. A
/// per-process monotonic sequence (plus the pid, to disambiguate cross-process
/// leftovers) closes that: a temp name is never handed out twice, so a leaked
/// temp is inert — nothing ever reopens it.
fn temp_sibling(full: &Utf8Path) -> Utf8PathBuf {
    let mut p = full.to_path_buf();
    let stem = p.file_name().unwrap_or("doc").to_string();
    let pid = std::process::id();
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    p.set_file_name(format!(".{stem}.{pid}-{seq}.tmp"));
    p
}

/// Fsync the directory containing `full` so a rename (a directory-entry change)
/// is itself durable across a crash (NRN-159). Best-effort: the rename has
/// already succeeded by the time this runs, so a fsync failure here must NOT
/// turn a completed write into a reported failure.
///
/// Unix only. Directory fsync is a POSIX construct with no portable Windows
/// equivalent — you cannot open a directory as a `File` to sync it there — so
/// the non-unix build is a documented no-op and leaves rename durability to the
/// filesystem.
#[cfg(unix)]
fn sync_parent_dir(full: &Utf8Path) {
    let parent = full.parent().filter(|p| !p.as_str().is_empty());
    let dir = parent.unwrap_or_else(|| Utf8Path::new("."));
    if let Ok(f) = fs::File::open(dir.as_std_path()) {
        let _ = f.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_parent_dir(_full: &Utf8Path) {
    // No portable directory fsync on non-unix; see the unix doc above.
}

/// Crash-atomic, durable write: stage `contents` into a sibling temp file
/// (`.{stem}.tmp`), fsync the temp, then `fs::rename` it into place (atomic on
/// POSIX) and fsync the parent directory. A SIGKILL / power loss / `ENOSPC`
/// mid-write truncates only the throwaway temp, never the live document — which
/// is exactly the half-mutation NRN-139 exists to prevent. Best-effort temp
/// cleanup on rename failure. Shared by the Phase A content write, the
/// `create_document` overwrite path, and (NRN-146) backlink-cascade rewrites in
/// `rewrite_one_backlink` so there is a single implementation for every document
/// write path. A side effect for cascade callers: renaming into place REPLACES a
/// symlink at `full` rather than writing through it, closing the symlink-file
/// cascade class NRN-145 could otherwise only gate at preflight.
///
/// Durability (NRN-159): an atomic rename only guarantees the NAME flips
/// atomically, not that the temp's bytes reached stable storage first. Without
/// the temp `sync_all`, a crash after the rename can leave the live name
/// pointing at a zero-length or torn temp — the very half-write the temp+rename
/// exists to prevent. The parent-dir fsync makes the rename itself durable. The
/// temp fsync propagates its error (nothing has been renamed yet, so failing is
/// safe and leaves only the throwaway); the dir fsync is best-effort (the write
/// already landed). Windows: temp fsync via `sync_all` still applies; the
/// directory fsync is a no-op (see [`sync_parent_dir`]).
///
/// Mode preservation: renaming a temp file over `full` replaces its inode, so
/// the replacement would otherwise pick up fresh umask-based permissions
/// rather than inheriting whatever mode the file it replaces had — silently
/// downgrading a permission-hardened document (e.g. `chmod 600`, see
/// docs/cache.md) on every incidental content rewrite or cascade touch. When
/// `full` already exists, stat it first and carry its mode over to the temp
/// file before the rename so the replacement's mode matches the original.
/// When `full` does not exist (a fresh `create_document`), there is nothing to
/// preserve — the temp file's default (umask-based) permissions are correct.
/// Best-effort only: a metadata-read or chmod failure falls back to the
/// unmodified temp permissions rather than failing the write — preserving
/// mode is hardening, not a new way for a rewrite to fail. Ownership/ACLs are
/// out of scope: unlike mode bits, they cannot be portably preserved without
/// root, so this covers the meaningful, portable subset.
pub(crate) fn atomic_write(full: &Utf8Path, contents: &str) -> std::io::Result<()> {
    let tmp_path = temp_sibling(full);
    {
        let mut f = fs::File::create(tmp_path.as_std_path())?;
        f.write_all(contents.as_bytes())?;
        // Durability: the temp's bytes must be on stable storage BEFORE the
        // rename publishes its name (NRN-159).
        f.sync_all()?;
    }
    #[cfg(unix)]
    if let Ok(existing) = fs::metadata(full.as_std_path()) {
        let _ = fs::set_permissions(tmp_path.as_std_path(), existing.permissions());
    }
    if let Err(e) = fs::rename(tmp_path.as_std_path(), full.as_std_path()) {
        // Best-effort cleanup on rename failure. Swallowing the cleanup error is
        // safe ONLY because temp names are never reused (see `temp_sibling`): a
        // leaked temp is inert, so it can never be reopened and truncated.
        let _ = fs::remove_file(tmp_path.as_std_path());
        return Err(e);
    }
    sync_parent_dir(full);
    Ok(())
}

/// Materialize a fresh `create_document` at `full` with crash-atomic durability
/// (NRN-159) and, unless `force`, no-clobber semantics (NRN-160).
///
/// `force == true` keeps the historical overwrite semantics: it is exactly
/// [`atomic_write`] (temp + rename, which replaces whatever is at `full`, and
/// inherits the durability + mode-preservation of that primitive).
///
/// `force == false` must NOT clobber a file that sprang into existence AFTER the
/// caller's friendly exists-precheck — the create TOCTOU window (NRN-160). std
/// offers no atomic rename-without-replace, so the strongest portable
/// no-clobber primitive is [`fs::hard_link`]: it fails with `AlreadyExists` when
/// the destination exists, atomically, on both unix and Windows. The sequence
/// is: stage the sibling temp and fsync it, then `hard_link` it into place; on
/// success unlink the temp and fsync the parent directory. A racer that wins the
/// window makes `hard_link` return `AlreadyExists`, which the caller maps to the
/// same "destination already exists" refusal — no byte of the racer's file is
/// overwritten.
///
/// Residual window: none for the clobber itself — `hard_link` is
/// atomic-exclusive, so the exists-precheck is only a nicer message for the
/// common no-race case, not a correctness dependency. Concurrent non-force
/// creates of the SAME path share the temp name; norn's own writers are
/// serialized by the owner's mutation lock, and the `hard_link` exclusivity
/// still guarantees at most one create wins against any foreign racer.
pub(crate) fn create_document_file(
    full: &Utf8Path,
    contents: &str,
    force: bool,
) -> std::io::Result<()> {
    if force {
        return atomic_write(full, contents);
    }
    let tmp_path = temp_sibling(full);
    {
        let mut f = fs::File::create(tmp_path.as_std_path())?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
    }
    match fs::hard_link(tmp_path.as_std_path(), full.as_std_path()) {
        Ok(()) => {
            // Swallowing this cleanup error is safe ONLY because temp names are
            // never reused (see `temp_sibling`): were a leaked temp reopened by a
            // later same-stem write, `File::create`'s O_TRUNC would truncate the
            // live document through the shared inode. Unique names make it inert.
            let _ = fs::remove_file(tmp_path.as_std_path());
            sync_parent_dir(full);
            Ok(())
        }
        Err(e) => {
            // AlreadyExists is the no-clobber refusal; anything else is a real IO
            // failure. Either way the temp is throwaway — clean it up (same
            // never-reused-so-safe-to-swallow reasoning as the success arm).
            let _ = fs::remove_file(tmp_path.as_std_path());
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planned(operation: &str, path: &str, destination: Option<&str>, force: bool) -> ApplyOp {
        ApplyOp {
            change_id: format!("{operation}-{path}"),
            path: Utf8PathBuf::from(path),
            document_hash: "irrelevant".into(),
            finding_code: None,
            finding_rule: None,
            repair_rule: None,
            operation: operation.into(),
            field: None,
            expected_old_value: None,
            new_value: None,
            destination: destination.map(Utf8PathBuf::from),
            link_risk: None,
            warnings: Vec::new(),
            force,
            parents: false,
        }
    }

    // ── atomic_write ────────────────────────────────────────────────────────

    #[test]
    fn atomic_write_lands_content_and_leaves_no_temp() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let full = root.join("doc.md");
        atomic_write(&full, "hello\n").unwrap();
        assert_eq!(fs::read_to_string(full.as_std_path()).unwrap(), "hello\n");

        // No sibling temp left behind: the temp+rename mechanism cleaned up.
        let leftovers: Vec<String> = fs::read_dir(root.as_std_path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with('.') && n.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "no .tmp sibling should remain after a successful atomic write; found: {leftovers:?}"
        );
    }

    #[test]
    fn atomic_write_overwrites_existing_and_syncs() {
        // fsync-path smoke: the write must complete and be readable back with the
        // new content (a crash-durability property we can only observe indirectly
        // by confirming the atomic replace succeeded end-to-end).
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let full = root.join("doc.md");
        atomic_write(&full, "first\n").unwrap();
        atomic_write(&full, "second\n").unwrap();
        assert_eq!(fs::read_to_string(full.as_std_path()).unwrap(), "second\n");
    }

    #[test]
    #[cfg(unix)]
    fn atomic_write_preserves_existing_destination_mode() {
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let full = root.join("doc.md");
        fs::write(full.as_std_path(), "orig\n").unwrap();
        fs::set_permissions(full.as_std_path(), Permissions::from_mode(0o600)).unwrap();

        atomic_write(&full, "new\n").unwrap();

        let mode = fs::metadata(full.as_std_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "atomic_write must carry the replaced file's mode over"
        );
    }

    // ── create_document_file (NRN-160 no-clobber) ───────────────────────────

    #[test]
    fn create_document_file_writes_fresh_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let full = root.join("new.md");
        create_document_file(&full, "body\n", false).unwrap();
        assert_eq!(fs::read_to_string(full.as_std_path()).unwrap(), "body\n");
    }

    /// A racer wins the create window: the destination already exists when the
    /// no-force materialization runs. `hard_link` must refuse with
    /// `AlreadyExists` and leave the racer's bytes intact — never clobbered.
    #[test]
    fn create_document_file_refuses_to_clobber_a_racer() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let full = root.join("contested.md");
        // Simulate the racer that appeared between the caller's precheck and here.
        fs::write(full.as_std_path(), "racer wins\n").unwrap();

        let err = create_document_file(&full, "would clobber\n", false).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read_to_string(full.as_std_path()).unwrap(),
            "racer wins\n",
            "no-clobber create must not overwrite a file that appeared in the window"
        );
        // No temp left behind after the refusal.
        let leftovers: Vec<String> = fs::read_dir(root.as_std_path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with('.') && n.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "refusal must clean its temp: {leftovers:?}"
        );
    }

    /// Regression (NRN-406 review): a LEAKED temp from a prior write — one whose
    /// best-effort cleanup failed, leaving it hard-linked to the LIVE inode — must
    /// never be reopened by a later same-stem write. With the old deterministic
    /// `.{stem}.tmp` name, `create_document_file`'s `File::create` would reopen
    /// that leftover `O_TRUNC` and truncate the live document through the shared
    /// inode (then refuse "destination already exists" on top). Unique temp names
    /// make the leftover inert. This test manually reconstructs the OLD name and
    /// asserts the live bytes survive — it FAILS against deterministic naming.
    #[test]
    #[cfg(unix)]
    fn create_does_not_truncate_live_doc_through_a_stale_deterministic_temp() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let full = root.join("doc.md");
        fs::write(full.as_std_path(), "live bytes\n").unwrap();

        // Simulate the leaked temp: the OLD deterministic name, hard-linked to the
        // live inode (exactly the shape a failed post-write cleanup would leave).
        let stale_tmp = root.join(".doc.md.tmp");
        fs::hard_link(full.as_std_path(), stale_tmp.as_std_path()).unwrap();

        // A fresh no-force create for the same stem. It must refuse (destination
        // exists) WITHOUT touching the live inode.
        let err = create_document_file(&full, "would clobber\n", false).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read_to_string(full.as_std_path()).unwrap(),
            "live bytes\n",
            "a stale same-name temp must never be reopened and truncate the live doc"
        );
        // The stale leftover is still inert, still hard-linked to the live bytes.
        assert_eq!(
            fs::read_to_string(stale_tmp.as_std_path()).unwrap(),
            "live bytes\n"
        );
    }

    /// `force == true` keeps overwrite semantics (it delegates to `atomic_write`).
    #[test]
    fn create_document_file_force_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let full = root.join("contested.md");
        fs::write(full.as_std_path(), "old\n").unwrap();
        create_document_file(&full, "new\n", true).unwrap();
        assert_eq!(fs::read_to_string(full.as_std_path()).unwrap(), "new\n");
    }

    #[test]
    #[cfg(unix)]
    fn create_document_file_fresh_gets_umask_default_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let full = root.join("new.md");
        create_document_file(&full, "body\n", false).unwrap();
        let mode = fs::metadata(full.as_std_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        // Ordinary umask-based mode (not 0); the exact bits depend on umask, so
        // assert only that it is a plausible readable-file mode.
        assert!(
            mode != 0,
            "fresh create must get ordinary permissions, got {mode:o}"
        );
    }

    // ── apply_delete ────────────────────────────────────────────────────────

    #[test]
    fn apply_delete_removes_file() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-apply-delete-")
            .tempdir()
            .unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let doc_rel = Utf8PathBuf::from("foo.md");
        fs::write(root.join(&doc_rel), "---\ntype: note\n---\n# Foo\n").unwrap();

        let change = planned("delete_document", "foo.md", None, false);
        let result = apply_delete(root, &change).unwrap();
        assert_eq!(result.path, doc_rel);
        assert!(!root.join(&doc_rel).as_std_path().exists());
    }

    #[test]
    fn apply_delete_missing_source_errors() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-apply-delete-missing-")
            .tempdir()
            .unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let change = planned("delete_document", "missing.md", None, false);
        let err = apply_delete(root, &change).unwrap_err();
        match err {
            ApplyError::DeleteSourceMissing { path } => assert_eq!(path.as_str(), "missing.md"),
            other => panic!("expected DeleteSourceMissing, got {other:?}"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn apply_delete_refuses_symlink() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-apply-delete-symlink-")
            .tempdir()
            .unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let real_rel = Utf8PathBuf::from("real.md");
        let link_rel = Utf8PathBuf::from("link.md");
        fs::write(root.join(&real_rel), "real").unwrap();
        std::os::unix::fs::symlink(root.join(&real_rel), root.join(&link_rel)).unwrap();

        let change = planned("delete_document", "link.md", None, false);
        let err = apply_delete(root, &change).unwrap_err();
        match err {
            ApplyError::DeleteSourceIsSymlink { path } => assert_eq!(path.as_str(), "link.md"),
            other => panic!("expected DeleteSourceIsSymlink, got {other:?}"),
        }
    }

    // ── apply_move ──────────────────────────────────────────────────────────

    #[test]
    fn apply_move_with_force_overwrites_destination() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-apply-move-force-")
            .tempdir()
            .unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let src_rel = Utf8PathBuf::from("src.md");
        let dst_rel = Utf8PathBuf::from("dst.md");
        fs::write(root.join(&src_rel), "src content").unwrap();
        fs::write(root.join(&dst_rel), "dst content").unwrap();

        let change = planned("move_document", "src.md", Some("dst.md"), true);
        let result = apply_move(root, &change).unwrap();
        assert_eq!(result.from, src_rel);
        assert_eq!(result.to, dst_rel);
        assert_eq!(
            fs::read_to_string(root.join(&dst_rel)).unwrap(),
            "src content"
        );
        assert!(!root.join(&src_rel).as_std_path().exists());
    }

    #[test]
    fn apply_move_without_force_refuses_existing_destination() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-apply-move-noforce-")
            .tempdir()
            .unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        fs::write(root.join("src.md"), "src").unwrap();
        fs::write(root.join("dst.md"), "dst").unwrap();

        let change = planned("move_document", "src.md", Some("dst.md"), false);
        let err = apply_move(root, &change).unwrap_err();
        match err {
            ApplyError::MoveDestinationExists { destination } => {
                assert_eq!(destination.as_str(), "dst.md")
            }
            other => panic!("expected MoveDestinationExists, got {other:?}"),
        }
    }

    // ── ensure_within_vault ─────────────────────────────────────────────────

    #[test]
    fn ensure_within_vault_accepts_in_vault_relative_target() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let canonical = root.as_std_path().canonicalize().unwrap();
        assert!(ensure_within_vault(root, &canonical, Utf8Path::new("notes/a.md")).is_ok());
    }

    #[test]
    fn ensure_within_vault_refuses_absolute() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let canonical = root.as_std_path().canonicalize().unwrap();
        let err = ensure_within_vault(root, &canonical, Utf8Path::new("/etc/passwd")).unwrap_err();
        assert!(matches!(err, ContainmentError::AbsolutePath { .. }));
    }

    #[test]
    fn ensure_within_vault_refuses_parent_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let canonical = root.as_std_path().canonicalize().unwrap();
        let err = ensure_within_vault(root, &canonical, Utf8Path::new("../escape.md")).unwrap_err();
        assert!(matches!(err, ContainmentError::ParentTraversal { .. }));
    }
}
