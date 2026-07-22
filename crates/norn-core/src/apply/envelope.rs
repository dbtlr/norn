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

/// A typed engine error that carries a stable, machine-branchable [`ApplyError`]
/// `code` (and, when the fault is about a specific document, a `path`).
///
/// # Single registration (NRN-236)
///
/// Registering a new error family with the apply envelope used to mean editing
/// THREE lockstep seams — the variant, its `code()` arm, and a bespoke
/// `downcast_ref` arm in [`from_anyhow`] — with nothing enforcing that all three
/// landed. A family that missed the third seam silently flattened to
/// `internal-error` (exactly the NRN-436 bug). This trait collapses the third
/// seam: a family implements `CodedError`, adds ONE line to [`REGISTRY`], and
/// [`from_anyhow`] recovers it uniformly. The
/// `every_registered_family_maps_off_internal_error` guard test constructs one
/// instance of every registered family and asserts none flattens to
/// `internal-error`, so a half-landed family cannot ship.
pub trait CodedError {
    /// The stable kebab code for this fault (the `code` of the envelope).
    fn code(&self) -> &'static str;
    /// The vault-relative path this fault is about, when it carries one. Feeds the
    /// envelope's `path`; defaults to `None`.
    fn error_path(&self) -> Option<String> {
        None
    }
}

impl CodedError for crate::standards::apply::ApplyError {
    fn code(&self) -> &'static str {
        crate::standards::apply::ApplyError::code(self)
    }
    fn error_path(&self) -> Option<String> {
        self.path().map(|p| p.to_string())
    }
}

impl CodedError for crate::standards::apply::ContainmentError {
    fn code(&self) -> &'static str {
        crate::standards::apply::ContainmentError::code(self)
    }
    fn error_path(&self) -> Option<String> {
        Some(self.target().to_string())
    }
}

impl CodedError for crate::planner::intent::rewrite_wikilink::RewriteWikilinkError {
    fn code(&self) -> &'static str {
        crate::planner::intent::rewrite_wikilink::RewriteWikilinkError::code(self)
    }
}

impl CodedError for crate::edit::transform::EditError {
    fn code(&self) -> &'static str {
        crate::edit::transform::EditError::code(self)
    }
    fn error_path(&self) -> Option<String> {
        self.path().map(str::to_string)
    }
}

impl CodedError for crate::apply::preconditions::PreconditionError {
    fn code(&self) -> &'static str {
        crate::apply::preconditions::PreconditionError::code(self)
    }
}

impl CodedError for crate::standards::apply::PlanStructureError {
    fn code(&self) -> &'static str {
        crate::standards::apply::PlanStructureError::code(self)
    }
    fn error_path(&self) -> Option<String> {
        self.path().map(|p| p.to_string())
    }
}

// A malformed AUTHORED op payload (unknown kind, a structural op missing a
// required field, non-object `fields`, a `kind`/`operation` mismatch, or a
// wrong-typed `fields` member) is a USER error, not a norn bug (NRN-405).
impl CodedError for norn_wire::TypedOpError {
    fn code(&self) -> &'static str {
        match self {
            norn_wire::TypedOpError::UnknownKind(_) => "unknown-operation-kind",
            norn_wire::TypedOpError::MissingField { .. }
            | norn_wire::TypedOpError::FieldsNotObject { .. }
            | norn_wire::TypedOpError::OperationKindMismatch { .. }
            | norn_wire::TypedOpError::MalformedFields { .. } => "malformed-plan",
        }
    }
}

/// One registered downcast attempt: recover `E` from an opaque `anyhow::Error`
/// and project it onto the [`ApplyError`] envelope through its [`CodedError`]
/// impl. `REGISTRY` is a list of these, one per family.
fn recover<E>(e: &anyhow::Error) -> Option<ApplyError>
where
    E: CodedError + std::fmt::Display + std::fmt::Debug + Send + Sync + 'static,
{
    e.downcast_ref::<E>().map(|typed| ApplyError {
        code: typed.code().to_string(),
        message: typed.to_string(),
        path: typed.error_path(),
    })
}

/// Every engine-owned error family the apply envelope recognizes. Adding a family
/// is ONE line here (plus its `CodedError` impl); the guard test enforces that no
/// registered family flattens to `internal-error`. Order matters only for the one
/// nesting case: [`ApplyError`](crate::standards::apply::ApplyError) is tried
/// before [`ContainmentError`](crate::standards::apply::ContainmentError) so a
/// `ApplyError::Containment` wrapper is projected through the outer type (it
/// delegates to the inner code/path), never shadowed.
#[allow(clippy::type_complexity)]
const REGISTRY: &[fn(&anyhow::Error) -> Option<ApplyError>] = &[
    recover::<crate::standards::apply::ApplyError>,
    recover::<crate::standards::apply::ContainmentError>,
    recover::<crate::planner::intent::rewrite_wikilink::RewriteWikilinkError>,
    recover::<crate::edit::transform::EditError>,
    recover::<crate::apply::preconditions::PreconditionError>,
    recover::<crate::standards::apply::PlanStructureError>,
    recover::<norn_wire::TypedOpError>,
];

/// Build the envelope from the rich apply-time error (NRN-150).
pub fn from_rich(e: &crate::standards::apply::ApplyError) -> ApplyError {
    ApplyError {
        code: CodedError::code(e).to_string(),
        message: e.to_string(),
        path: e.error_path(),
    }
}

/// Build the envelope from a containment error (path escaped the vault root).
pub fn from_containment(e: &crate::standards::apply::ContainmentError) -> ApplyError {
    ApplyError {
        code: CodedError::code(e).to_string(),
        message: e.to_string(),
        path: e.error_path(),
    }
}

/// Build the envelope from an opaque `anyhow::Error`, recovering structure by
/// walking the [`REGISTRY`] of engine-owned families. Falls back to a generic
/// `internal-error` code for anything unrecognized so a report consumer ALWAYS
/// gets `{ code, message }`, never a bare exit + prose.
pub fn from_anyhow(e: &anyhow::Error) -> ApplyError {
    for recover in REGISTRY {
        if let Some(envelope) = recover(e) {
            return envelope;
        }
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

    /// The single-registration guard (NRN-236): one instance of EVERY family in
    /// [`REGISTRY`] must recover to its own code through [`from_anyhow`] — none may
    /// flatten to `internal-error`. A family added to `REGISTRY` (or a new family
    /// added WITHOUT registering it) is caught here the moment its representative
    /// is listed, so a half-landed family (the NRN-436 bug — a typed error the
    /// envelope silently flattens) cannot ship. Keep one entry per registered
    /// family; the `assert_ne` is the load-bearing check, the `assert_eq` documents
    /// the code each family lands.
    #[test]
    fn every_registered_family_maps_off_internal_error() {
        let cases: Vec<(anyhow::Error, &str)> = vec![
            (
                crate::standards::apply::ApplyError::UnknownPath {
                    path: camino::Utf8PathBuf::from("missing.md"),
                }
                .into(),
                "unknown-path",
            ),
            (
                crate::standards::apply::ContainmentError::AbsolutePath {
                    target: camino::Utf8PathBuf::from("/etc/passwd"),
                }
                .into(),
                "containment-absolute-path",
            ),
            (
                crate::planner::intent::rewrite_wikilink::RewriteWikilinkError::OldUnresolved(
                    "ghost".into(),
                )
                .into(),
                "target-not-found",
            ),
            (
                crate::edit::transform::EditError::InvalidOp {
                    index: 0,
                    kind: "str_replace",
                }
                .into(),
                "empty-anchor",
            ),
            (
                crate::apply::preconditions::PreconditionError::DuplicateId { id: "p".into() }
                    .into(),
                "invalid-precondition",
            ),
            (
                crate::standards::apply::PlanStructureError::DuplicateOperationId {
                    id: "op1".into(),
                }
                .into(),
                "malformed-plan",
            ),
            (
                norn_wire::TypedOpError::UnknownKind("no_such_kind".into()).into(),
                "unknown-operation-kind",
            ),
        ];
        assert_eq!(
            cases.len(),
            REGISTRY.len(),
            "the guard must list one representative per registered family"
        );
        for (err, expected_code) in cases {
            let envelope = from_anyhow(&err);
            assert_ne!(
                envelope.code, "internal-error",
                "a registered family flattened to internal-error: {err:?}"
            );
            assert_eq!(envelope.code, expected_code, "for {err:?}");
        }
    }

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
    fn from_anyhow_codes_delete_hash_required_with_path() {
        // The delete-hash fault carries its OWN dedicated code (not the generic
        // `malformed-plan`) plus the offending vault-relative path, so an agent
        // branches the precise "stamp the hash" remedy (NRN-151, ADR 0024).
        let err: anyhow::Error = crate::standards::apply::PlanStructureError::DeleteHashRequired {
            path: camino::Utf8PathBuf::from("notes/alpha.md"),
        }
        .into();
        let envelope = from_anyhow(&err);
        assert_eq!(envelope.code, "delete-hash-required");
        assert_eq!(envelope.path.as_deref(), Some("notes/alpha.md"));
        assert!(envelope.message.contains("no document_hash"));
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
