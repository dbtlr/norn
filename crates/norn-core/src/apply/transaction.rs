//! Per-file mutation transaction: fingerprint → shadow → verify → swap.
//!
//! A pass hands one document's composed transforms to [`run_content_transaction`],
//! which turns the read-compose-write sequence into a single unit with two drift
//! guards the old two-phase applier lacked:
//!
//! 1. **Fingerprint CAS against file bytes.** The plan's `document_hash` is
//!    checked against the file's ACTUAL bytes read here, not the GraphIndex
//!    snapshot taken at orchestrator entry. The index can be stale relative to
//!    the file; comparing against the bytes we are about to transform strictly
//!    catches MORE staleness (never less) — a plan computed against a document
//!    that changed on disk since the index was built refuses as stale rather
//!    than silently misapplying.
//! 2. **Swap re-read.** Immediately before the atomic rename, the file is
//!    re-read and compared to the fingerprint. norn's own writers are serialized
//!    by the owner's mutation lock, so a difference here is a FOREIGN writer that
//!    landed during the compose window. Declarative edits (independent of
//!    surrounding content) bounded-retry against the new content; content-anchored
//!    edits refuse with `concurrent-modification` rather than destroy the
//!    external change.
//!
//! The hash algorithm is exactly the graph index's and the plan's
//! (`blake3::hash(content.as_bytes())`, see `graph::build`), so a plan-carried
//! `document_hash` still compares equal against a freshly-read file.

use anyhow::Context;
use camino::Utf8Path;

use crate::apply::fsops;
use crate::standards::apply::ApplyError;

/// Blake3 hex of the file's UTF-8 content — the SAME digest the graph index and
/// repair plans carry, so plan-carried `document_hash` values compare equal.
pub(crate) fn content_hash(content: &str) -> String {
    blake3::hash(content.as_bytes()).to_hex().to_string()
}

/// How a per-file transaction reacts to an external write landing between the
/// fingerprint read and the swap.
pub(crate) enum DriftPolicy {
    /// Every op on the file is declarative (set/add/remove frontmatter,
    /// strip_bom): the edit is independent of surrounding content, so on drift
    /// re-run the whole transaction against the new content, bounded to
    /// `max_attempts` rounds before giving up with `concurrent-modification`.
    ///
    /// The bound interacts with the fingerprint CAS: if the plan carried a
    /// `document_hash`, a drifted file no longer matches it, so the NEXT
    /// attempt's CAS refuses as `stale-document-hash` (a changed file IS a stale
    /// plan) — the bounded re-land only actually re-lands for operator-originated
    /// plans that carry no hash. That is intentional: a hash on the plan is a
    /// contract that the edits were computed against exactly those bytes, and a
    /// mid-apply change breaks it.
    RetryDeclarative { max_attempts: usize },
    /// At least one op is content-anchored (str_replace, section/heading ops,
    /// rewrite_link, replace_body) or structural: re-landing on drifted content
    /// would silently destroy the external edit, so refuse immediately with
    /// `concurrent-modification`. No retry. (RULED: replace_body is
    /// content-anchored — a wholesale overwrite on drifted content destroys the
    /// external edit; heading-anchored ops are content-anchored, fail-safe.)
    RefuseContentAnchored,
}

/// The shadow the caller composed over the fingerprinted content, plus whatever
/// per-file payload the caller needs back (e.g. the report forecast units).
pub(crate) struct Composition<P> {
    /// The fully-composed new content to write.
    pub content: String,
    /// Whether the composition actually mutated the file — the write predicate.
    /// A byte-identical composition writes nothing (and skips the swap guard).
    pub changed: bool,
    /// Caller payload carried through unchanged.
    pub payload: P,
}

/// Result of a committed transaction.
#[derive(Debug)]
pub(crate) struct Committed<P> {
    /// Whether a write actually landed (false for a no-op composition).
    pub wrote: bool,
    /// The payload from the winning compose attempt.
    pub payload: P,
}

/// Read the file and confirm the plan's `document_hash` matches its actual
/// bytes. An empty `plan_hash` is operator-originated (no CAS). Returns the file
/// content on success. The single home of the file-bytes CAS.
pub(crate) fn fingerprint_cas(
    abs_path: &Utf8Path,
    rel_path: &Utf8Path,
    plan_hash: &str,
) -> anyhow::Result<String> {
    let content = std::fs::read_to_string(abs_path.as_std_path())
        .with_context(|| format!("read {abs_path}"))?;
    if !plan_hash.is_empty() {
        let actual = content_hash(&content);
        if actual != plan_hash {
            return Err(anyhow::anyhow!(ApplyError::StaleDocumentHash {
                path: rel_path.to_path_buf(),
                expected: plan_hash.to_string(),
                actual,
            }));
        }
    }
    Ok(content)
}

/// Fingerprint CAS for a delete: if the file reads, its bytes must match the
/// plan hash; a missing/unreadable file is left for `apply_delete` to report
/// with its precise `delete-source-missing` refusal rather than a read error.
/// An empty `plan_hash` skips the check (the op carried no hash).
pub(crate) fn fingerprint_delete(
    abs_path: &Utf8Path,
    rel_path: &Utf8Path,
    plan_hash: &str,
) -> anyhow::Result<()> {
    if plan_hash.is_empty() {
        return Ok(());
    }
    let Ok(content) = std::fs::read_to_string(abs_path.as_std_path()) else {
        return Ok(());
    };
    let actual = content_hash(&content);
    if actual != plan_hash {
        return Err(anyhow::anyhow!(ApplyError::StaleDocumentHash {
            path: rel_path.to_path_buf(),
            expected: plan_hash.to_string(),
            actual,
        }));
    }
    Ok(())
}

/// Run one file's content transaction: fingerprint → shadow (`compose`) → verify
/// (inside `compose`) → swap. `abs_path` is the on-disk file, `rel_path` names
/// it in errors, `plan_hash` is the plan CAS ("" = none). `compose` produces the
/// new content + a caller payload from the current file content; it runs the
/// pre-write gates and may fail deterministically, which aborts with no write
/// and no retry (a deterministic transform failure is not drift).
pub(crate) fn run_content_transaction<P>(
    abs_path: &Utf8Path,
    rel_path: &Utf8Path,
    plan_hash: &str,
    policy: DriftPolicy,
    mut compose: impl FnMut(&str) -> anyhow::Result<Composition<P>>,
) -> anyhow::Result<Committed<P>> {
    let mut attempt = 0usize;
    loop {
        attempt += 1;
        // Fingerprint: read the actual bytes ONCE and CAS against them.
        let original = fingerprint_cas(abs_path, rel_path, plan_hash)?;
        // Shadow + verify: compose the transforms over the in-memory copy.
        let comp = compose(&original)?;
        if !comp.changed {
            // A byte-identical composition never writes and cannot drift-corrupt.
            return Ok(Committed {
                wrote: false,
                payload: comp.payload,
            });
        }
        // Swap: re-read immediately before the rename and compare to the
        // fingerprint. A difference is a foreign writer (norn's own writers are
        // serialized by the owner's mutation lock).
        let now = std::fs::read_to_string(abs_path.as_std_path())
            .with_context(|| format!("re-read {abs_path} before swap"))?;
        if now == original {
            // Residual window: another process could still write between this
            // re-read and the rename below. std has no atomic file
            // compare-and-swap; the mutation lock closes it for norn's own
            // writers, and the re-read shrinks it to a sub-millisecond gap for
            // any foreign writer.
            fsops::atomic_write(abs_path, &comp.content)
                .with_context(|| format!("write {abs_path}"))?;
            return Ok(Committed {
                wrote: true,
                payload: comp.payload,
            });
        }
        // Drift: the file changed under us during compose.
        match policy {
            DriftPolicy::RefuseContentAnchored => {
                return Err(anyhow::anyhow!(ApplyError::ConcurrentModification {
                    path: rel_path.to_path_buf(),
                }));
            }
            DriftPolicy::RetryDeclarative { max_attempts } => {
                if attempt >= max_attempts {
                    return Err(anyhow::anyhow!(ApplyError::ConcurrentModification {
                        path: rel_path.to_path_buf(),
                    }));
                }
                // Loop: the next iteration re-fingerprints. If the plan carried a
                // hash, that CAS now refuses against the changed file
                // (stale-document-hash); if it carried none, we re-land against
                // the new content.
                continue;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A compose closure that appends a frontmatter-ish line — stands in for a
    /// declarative edit whose result is independent of surrounding content.
    fn declarative_compose(
        new_body: &str,
    ) -> impl Fn(&str) -> anyhow::Result<Composition<()>> + '_ {
        move |content: &str| {
            let out = format!("{content}{new_body}");
            Ok(Composition {
                changed: out != content,
                content: out,
                payload: (),
            })
        }
    }

    #[test]
    fn clean_transaction_writes_composed_content() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let abs = root.join("a.md");
        std::fs::write(abs.as_std_path(), "start\n").unwrap();
        let hash = content_hash("start\n");

        let committed = run_content_transaction(
            &abs,
            Utf8Path::new("a.md"),
            &hash,
            DriftPolicy::RetryDeclarative { max_attempts: 3 },
            declarative_compose("added\n"),
        )
        .unwrap();
        assert!(committed.wrote);
        assert_eq!(
            std::fs::read_to_string(abs.as_std_path()).unwrap(),
            "start\nadded\n"
        );
    }

    #[test]
    fn cas_against_file_bytes_catches_index_stale_case() {
        // The plan carries a hash for content the file NO LONGER has (an index
        // built earlier saw the old bytes). The file-bytes CAS must refuse.
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let abs = root.join("a.md");
        std::fs::write(abs.as_std_path(), "current on disk\n").unwrap();
        let stale_plan_hash = content_hash("what the index thought\n");

        let err = run_content_transaction(
            &abs,
            Utf8Path::new("a.md"),
            &stale_plan_hash,
            DriftPolicy::RefuseContentAnchored,
            declarative_compose("x\n"),
        )
        .unwrap_err();
        let rich = err.downcast_ref::<ApplyError>().unwrap();
        assert_eq!(rich.code(), "stale-document-hash");
        assert_eq!(
            std::fs::read_to_string(abs.as_std_path()).unwrap(),
            "current on disk\n",
            "a stale-plan refusal writes nothing"
        );
    }

    #[test]
    fn declarative_retry_lands_after_external_change() {
        // No plan hash (operator-originated). A foreign writer changes the file
        // during the FIRST compose window; the second attempt re-lands cleanly.
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let abs = root.join("a.md");
        std::fs::write(abs.as_std_path(), "v1\n").unwrap();

        let abs_for_hook = abs.clone();
        let mut fired = false;
        let compose = move |content: &str| -> anyhow::Result<Composition<()>> {
            // Test seam: on the first attempt only, simulate an external writer
            // landing between fingerprint and swap by mutating the file here.
            if !fired {
                fired = true;
                std::fs::write(abs_for_hook.as_std_path(), "v2 external\n").unwrap();
            }
            let out = format!("{content}tag\n");
            Ok(Composition {
                changed: out != content,
                content: out,
                payload: (),
            })
        };

        let committed = run_content_transaction(
            &abs,
            Utf8Path::new("a.md"),
            "", // no CAS: operator-originated declarative edit
            DriftPolicy::RetryDeclarative { max_attempts: 3 },
            compose,
        )
        .unwrap();
        assert!(committed.wrote);
        // Second attempt composed against the external content.
        assert_eq!(
            std::fs::read_to_string(abs.as_std_path()).unwrap(),
            "v2 external\ntag\n"
        );
    }

    #[test]
    fn content_anchored_drift_refuses_with_concurrent_modification() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let abs = root.join("a.md");
        std::fs::write(abs.as_std_path(), "v1\n").unwrap();

        let abs_for_hook = abs.clone();
        let compose = move |content: &str| -> anyhow::Result<Composition<()>> {
            // External writer lands during the compose window.
            std::fs::write(abs_for_hook.as_std_path(), "v2 external\n").unwrap();
            let out = format!("{content}anchored\n");
            Ok(Composition {
                changed: out != content,
                content: out,
                payload: (),
            })
        };

        let err = run_content_transaction(
            &abs,
            Utf8Path::new("a.md"),
            "",
            DriftPolicy::RefuseContentAnchored,
            compose,
        )
        .unwrap_err();
        let rich = err.downcast_ref::<ApplyError>().unwrap();
        assert_eq!(rich.code(), "concurrent-modification");
        // Nothing written for the file beyond the simulated external write.
        assert_eq!(
            std::fs::read_to_string(abs.as_std_path()).unwrap(),
            "v2 external\n",
            "a content-anchored drift refusal writes nothing of its own"
        );
    }

    #[test]
    fn declarative_retry_bound_exhausts_to_concurrent_modification() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let abs = root.join("a.md");
        std::fs::write(abs.as_std_path(), "v0\n").unwrap();

        let abs_for_hook = abs.clone();
        let counter = std::cell::Cell::new(0u32);
        let compose = move |content: &str| -> anyhow::Result<Composition<()>> {
            // Every attempt sees a fresh external write → perpetual drift.
            let n = counter.get() + 1;
            counter.set(n);
            std::fs::write(abs_for_hook.as_std_path(), format!("ext{n}\n")).unwrap();
            let out = format!("{content}tag\n");
            Ok(Composition {
                changed: out != content,
                content: out,
                payload: (),
            })
        };

        let err = run_content_transaction(
            &abs,
            Utf8Path::new("a.md"),
            "",
            DriftPolicy::RetryDeclarative { max_attempts: 3 },
            compose,
        )
        .unwrap_err();
        let rich = err.downcast_ref::<ApplyError>().unwrap();
        assert_eq!(rich.code(), "concurrent-modification");
    }

    #[test]
    fn noop_composition_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let abs = root.join("a.md");
        std::fs::write(abs.as_std_path(), "same\n").unwrap();

        let committed = run_content_transaction(
            &abs,
            Utf8Path::new("a.md"),
            "",
            DriftPolicy::RefuseContentAnchored,
            |content: &str| {
                Ok(Composition {
                    content: content.to_string(),
                    changed: false,
                    payload: (),
                })
            },
        )
        .unwrap();
        assert!(!committed.wrote);
    }
}
