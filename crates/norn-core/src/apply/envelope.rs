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
//! `RewriteWikilinkError`, the section-edit [`EditError`](crate::edit::transform::EditError),
//! and the authored-plan-fault families
//! [`PreconditionError`](crate::apply::preconditions::PreconditionError) and
//! [`PlanStructureError`](crate::standards::apply::PlanStructureError), NRN-436),
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
    // Owner-set precondition-validation faults (duplicate id, empty stem/eq
    // selector, missing named op, unparseable eq predicate) — a malformed AUTHORED
    // precondition, coded `invalid-precondition`, not a norn bug (NRN-436).
    if let Some(precondition) = e.downcast_ref::<crate::apply::preconditions::PreconditionError>() {
        return ApplyError {
            code: precondition.code().to_string(),
            message: precondition.to_string(),
            path: None,
        };
    }
    // Plan-structure faults (duplicate op id, create path with no stem, edit
    // payload missing/undecodable) — a malformed AUTHORED plan, coded
    // `malformed-plan`, not a norn bug (NRN-436).
    if let Some(structure) = e.downcast_ref::<crate::standards::apply::PlanStructureError>() {
        return ApplyError {
            code: structure.code().to_string(),
            message: structure.to_string(),
            path: None,
        };
    }
    // A malformed AUTHORED plan (unknown kind, a structural op missing a required
    // field, non-object `fields`, or a `kind`/`operation` mismatch) is a USER
    // error, not a norn bug. Map each typed variant to a machine-branchable code
    // so a consumer can distinguish "your plan is malformed" from `internal-error`
    // ("norn has a bug"). The message is the typed error's own Display, unchanged.
    if let Some(typed) = e.downcast_ref::<norn_wire::TypedOpError>() {
        let code = match typed {
            norn_wire::TypedOpError::UnknownKind(_) => "unknown-operation-kind",
            norn_wire::TypedOpError::MissingField { .. }
            | norn_wire::TypedOpError::FieldsNotObject { .. }
            | norn_wire::TypedOpError::OperationKindMismatch { .. }
            | norn_wire::TypedOpError::MalformedFields { .. } => "malformed-plan",
        };
        return ApplyError {
            code: code.to_string(),
            message: typed.to_string(),
            path: None,
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
    fn from_anyhow_maps_unknown_kind_to_unknown_operation_kind() {
        let err: anyhow::Error = norn_wire::TypedOpError::UnknownKind("no_such_kind".into()).into();
        let envelope = from_anyhow(&err);
        assert_eq!(envelope.code, "unknown-operation-kind");
        assert_eq!(envelope.message, "unknown operation kind: no_such_kind");
    }

    #[test]
    fn from_anyhow_maps_missing_field_to_malformed_plan() {
        let err: anyhow::Error = norn_wire::TypedOpError::MissingField {
            kind: "move_document".into(),
            field: "src",
        }
        .into();
        let envelope = from_anyhow(&err);
        assert_eq!(envelope.code, "malformed-plan");
        assert_eq!(envelope.message, "move_document missing src");
    }

    #[test]
    fn from_anyhow_maps_fields_not_object_to_malformed_plan() {
        let err: anyhow::Error = norn_wire::TypedOpError::FieldsNotObject {
            kind: "set_frontmatter".into(),
        }
        .into();
        let envelope = from_anyhow(&err);
        assert_eq!(envelope.code, "malformed-plan");
        assert_eq!(
            envelope.message,
            "op.fields for set_frontmatter must be an object"
        );
    }

    #[test]
    fn from_anyhow_maps_operation_kind_mismatch_to_malformed_plan() {
        let err: anyhow::Error = norn_wire::TypedOpError::OperationKindMismatch {
            kind: "set_frontmatter".into(),
            operation: "remove_frontmatter".into(),
        }
        .into();
        let envelope = from_anyhow(&err);
        assert_eq!(envelope.code, "malformed-plan");
        assert_eq!(
            envelope.message,
            "op.fields.operation 'remove_frontmatter' conflicts with op.kind 'set_frontmatter'"
        );
    }

    #[test]
    fn from_anyhow_maps_malformed_fields_to_malformed_plan() {
        let err: anyhow::Error = norn_wire::TypedOpError::MalformedFields {
            kind: "set_frontmatter".into(),
            message: "invalid type: integer `5`, expected a string".into(),
        }
        .into();
        let envelope = from_anyhow(&err);
        assert_eq!(envelope.code, "malformed-plan");
        assert_eq!(
            envelope.message,
            "op.fields for set_frontmatter could not be decoded: invalid type: integer `5`, expected a string"
        );
    }

    #[test]
    fn from_anyhow_codes_create_destination_exists() {
        let err: anyhow::Error = crate::standards::apply::ApplyError::CreateDestinationExists {
            path: camino::Utf8PathBuf::from("seq/task-1.md"),
        }
        .into();
        let envelope = from_anyhow(&err);
        assert_eq!(envelope.code, "create-destination-exists");
        assert_eq!(envelope.path.as_deref(), Some("seq/task-1.md"));
        assert_eq!(
            envelope.message,
            "create_document: destination already exists (use --force to overwrite): seq/task-1.md"
        );
    }

    #[test]
    fn from_anyhow_codes_create_parent_missing() {
        let err: anyhow::Error = crate::standards::apply::ApplyError::CreateParentMissing {
            path: camino::Utf8PathBuf::from("seq/task-1.md"),
        }
        .into();
        assert_eq!(from_anyhow(&err).code, "create-parent-missing");
    }

    #[test]
    fn from_anyhow_codes_create_ignored_path() {
        let err: anyhow::Error = crate::standards::apply::ApplyError::CreateIgnoredPath {
            path: camino::Utf8PathBuf::from("logs/1.md"),
        }
        .into();
        assert_eq!(from_anyhow(&err).code, "create-ignored-path");
    }

    #[test]
    fn from_anyhow_codes_create_malformed_frontmatter_as_malformed_plan() {
        let err: anyhow::Error =
            crate::standards::apply::ApplyError::CreateFrontmatterMalformed.into();
        let envelope = from_anyhow(&err);
        assert_eq!(envelope.code, "malformed-plan");
        assert!(envelope.path.is_none());
        assert_eq!(
            envelope.message,
            "create_document: missing or non-object frontmatter in new_value"
        );
    }

    #[test]
    fn from_anyhow_codes_create_serialize_failed_as_malformed_plan() {
        let err: anyhow::Error = crate::standards::apply::ApplyError::CreateSerializeFailed {
            message: "boom".into(),
        }
        .into();
        let envelope = from_anyhow(&err);
        assert_eq!(envelope.code, "malformed-plan");
        assert_eq!(envelope.message, "create_document: serialize failed: boom");
    }

    #[test]
    fn from_anyhow_codes_precondition_faults_as_invalid_precondition() {
        for err in [
            crate::apply::preconditions::PreconditionError::DuplicateId { id: "p".into() },
            crate::apply::preconditions::PreconditionError::EmptyStemSelector { id: "p".into() },
            crate::apply::preconditions::PreconditionError::EmptyEqSelector { id: "p".into() },
            crate::apply::preconditions::PreconditionError::MissingOperation {
                id: "p".into(),
                operation: "op1".into(),
            },
            crate::apply::preconditions::PreconditionError::EqPredicateParse {
                message: "bad".into(),
            },
        ] {
            let envelope = from_anyhow(&err.into());
            assert_eq!(envelope.code, "invalid-precondition");
            assert!(envelope.path.is_none());
        }
    }

    #[test]
    fn from_anyhow_codes_plan_structure_faults_as_malformed_plan() {
        for err in [
            crate::standards::apply::PlanStructureError::DuplicateOperationId { id: "op1".into() },
            crate::standards::apply::PlanStructureError::CreatePathNoStem {
                path: camino::Utf8PathBuf::from(".md"),
            },
            crate::standards::apply::PlanStructureError::EditPayloadMissing {
                path: camino::Utf8PathBuf::from("a.md"),
            },
            crate::standards::apply::PlanStructureError::EditPayloadDecode {
                path: camino::Utf8PathBuf::from("a.md"),
                message: "bad".into(),
            },
        ] {
            let envelope = from_anyhow(&err.into());
            assert_eq!(envelope.code, "malformed-plan");
            assert!(envelope.path.is_none());
        }
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
