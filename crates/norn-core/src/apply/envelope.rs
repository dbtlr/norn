//! Engine-local error → coded [`ApplyError`] envelope conversions.
//!
//! The executor's refusal and partial-failure reports carry a coded
//! [`ApplyError`](norn_wire::ApplyError) envelope (`{code, message,
//! path?}`). These constructors build that envelope from the ENGINE-OWNED typed
//! errors norn-core produces.
//!
//! # Seam boundary (ADR 0018)
//!
//! The donor's full `from_anyhow` ladder downcast through the SURFACE / verb
//! error types too (`set::error::SetError`, `move`/`delete` preflight errors,
//! `service::PostSendUncertainError`, `cache::CacheError::MutationLockTimeout`).
//! Those are NOT re-homed here: the executor never receives them — the mutation
//! lock is the owner's, and the verb-preflight refusals are coded by their verbs,
//! which wrap the executor. This module downcasts only the types the mutation
//! engine itself raises ([`standards::apply::ApplyError`](crate::standards::apply::ApplyError),
//! [`ContainmentError`](crate::standards::apply::ContainmentError), the planner's
//! `RewriteWikilinkError`, and the section-edit [`EditError`](crate::edit::transform::EditError)),
//! falling back to a generic `internal-error` for anything unrecognized so a
//! refusal report always carries a coded envelope.

use norn_wire::ApplyError;

// The [`ApplyError`] envelope is the end-user contract and lives in norn-wire.
// These constructors need norn-core-internal engine error types, so they cannot
// be inherent impls on the wire type; they are free functions in this module
// (NRN-405). Call sites reach them as `crate::apply::envelope::from_anyhow` etc.

/// Build the envelope from the rich apply-time error (NRN-150).
pub fn from_rich(e: &crate::standards::apply::ApplyError) -> ApplyError {
    ApplyError {
        code: e.code().to_string(),
        message: e.to_string(),
        path: e.path().map(|p| p.to_string()),
    }
}

/// Build the envelope from a containment error (path escaped the vault root).
pub fn from_containment(e: &crate::standards::apply::ContainmentError) -> ApplyError {
    ApplyError {
        code: e.code().to_string(),
        message: e.to_string(),
        path: Some(e.target().to_string()),
    }
}

/// Build the envelope from an opaque `anyhow::Error`, recovering structure by
/// downcasting through the ENGINE-OWNED failure types. Falls back to a generic
/// `internal-error` code for anything unrecognized so a report consumer ALWAYS
/// gets `{ code, message }`, never a bare exit + prose.
pub fn from_anyhow(e: &anyhow::Error) -> ApplyError {
    if let Some(rich) = e.downcast_ref::<crate::standards::apply::ApplyError>() {
        return from_rich(rich);
    }
    if let Some(containment) = e.downcast_ref::<crate::standards::apply::ContainmentError>() {
        return from_containment(containment);
    }
    if let Some(rewrite) =
        e.downcast_ref::<crate::planner::intent::rewrite_wikilink::RewriteWikilinkError>()
    {
        return ApplyError {
            code: rewrite.code().to_string(),
            message: rewrite.to_string(),
            path: None,
        };
    }
    if let Some(edit) = e.downcast_ref::<crate::edit::transform::EditError>() {
        return ApplyError {
            code: edit.code().to_string(),
            message: edit.to_string(),
            path: edit.path().map(str::to_string),
        };
    }
    ApplyError {
        code: "internal-error".to_string(),
        // `{:#}` renders the full anyhow context chain.
        message: format!("{e:#}"),
        path: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_rich_carries_code_and_path() {
        let rich = crate::standards::apply::ApplyError::UnknownPath {
            path: camino::Utf8PathBuf::from("missing.md"),
        };
        let envelope = from_rich(&rich);
        assert_eq!(envelope.code, "unknown-path");
        assert_eq!(envelope.path.as_deref(), Some("missing.md"));
    }

    #[test]
    fn from_anyhow_recovers_the_rich_apply_code() {
        let err: anyhow::Error = crate::standards::apply::ApplyError::UnknownPath {
            path: camino::Utf8PathBuf::from("missing.md"),
        }
        .into();
        let envelope = from_anyhow(&err);
        assert_eq!(envelope.code, "unknown-path");
        assert_eq!(envelope.path.as_deref(), Some("missing.md"));
    }

    #[test]
    fn from_anyhow_recovers_containment_code() {
        let err: anyhow::Error = crate::standards::apply::ContainmentError::AbsolutePath {
            target: camino::Utf8PathBuf::from("/etc/passwd"),
        }
        .into();
        let envelope = from_anyhow(&err);
        assert_eq!(envelope.code, "containment-absolute-path");
        assert_eq!(envelope.path.as_deref(), Some("/etc/passwd"));
    }

    #[test]
    fn from_anyhow_falls_back_to_internal_error() {
        let err = anyhow::anyhow!("something unexpected");
        let envelope = from_anyhow(&err);
        assert_eq!(envelope.code, "internal-error");
        assert!(envelope.message.contains("something unexpected"));
        assert!(envelope.path.is_none());
    }
}
