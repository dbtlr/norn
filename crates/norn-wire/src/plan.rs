//! `MigrationPlan` — the typed-op plan model the mutation engine applies.
//!
//! A plan is a surface-neutral, serializable artifact: the mutation verbs
//! (`set`/`new`/`move`/`delete`/`rewrite-wikilink`) and the repair generator all
//! produce one, and the applier executes it. Schema v2 carries first-class
//! atomic owner-set preconditions (ADR 0015): logical-identity assertions proven
//! under the mutation lock before any operation writes.
//!
//! The plan crosses the wire as plan bytes (ADR 0011): what was reviewed is what
//! is applied. Its [`canonical_hash`](MigrationPlan::canonical_hash) identifies
//! the plan's content independent of its on-disk representation (JSON or YAML).

use serde::{Deserialize, Serialize};

/// Current `MigrationPlan` schema version. v2 added atomic owner-set
/// preconditions (ADR 0015).
pub const MIGRATION_PLAN_SCHEMA_VERSION: u32 = 2;

/// A reviewable, applyable set of typed operations over one vault, plus the
/// owner-set preconditions that must hold before any of them writes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MigrationPlan {
    pub schema_version: u32,
    pub vault_root: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub generator: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub generated_at: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub preconditions: Vec<PlanPrecondition>,
    pub operations: Vec<MigrationOp>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub skipped: Vec<SkippedFinding>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub plan_footnote: Option<String>,
}

/// A pre-write barrier evaluated once, under the mutation lock, before any
/// operation writes (ADR 0015). Today the only variant is an exact owner-set
/// assertion; the `kind`-tagged enum leaves room for future precondition
/// families without a schema break.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanPrecondition {
    OwnerSet {
        id: String,
        selector: OwnerSelector,
        expected_paths: Vec<String>,
    },
}

impl PlanPrecondition {
    pub fn id(&self) -> &str {
        match self {
            Self::OwnerSet { id, .. } => id,
        }
    }
}

/// How an owner-set precondition selects the current owners it will compare
/// against `expected_paths`. The three grammars are mutually exclusive
/// (`deny_unknown_fields` + `untagged`), so a plan cannot mix `stem` with `eq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum OwnerSelector {
    /// Every document whose filename stem matches (ASCII-case-folded).
    Stem { stem: String },
    /// Every document whose frontmatter satisfies every `field:value` predicate.
    Eq { eq: Vec<String> },
    /// The resolved stem of a named create operation in this same plan — used to
    /// assert an owner's ABSENCE before creating it.
    StemFromOperation { stem_from_operation: String },
}

/// One operation. On the wire / on disk, `kind` is a sibling of the `fields`
/// JSON payload (this shape is parity-pinned — an authored plan's `kind`+`fields`
/// layout is a contract). At the EXECUTION boundary the executor resolves an op
/// to the typed [`TypedOp`] vocabulary via `TypedOp::try_from(&op)`, so untyped
/// `fields` indexing does not flow into the applier (NRN-405 part b / ADR 0016).
///
/// `kind` stays a free `String` (not a closed enum) on purpose: an UNRECOGNIZED
/// kind must still PARSE — it is refused later, at the [`TypedOp`] conversion, so
/// the "unknown kind is an executor refusal, not a deserialize rejection" contract
/// holds (parity-pinned).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MigrationOp {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub requires: Vec<String>,
    pub fields: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub footnote: Option<String>,
}

/// A finding the plan generator chose not to act on, carried forward so the
/// apply report can surface it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkippedFinding {
    pub finding_code: String,
    pub path: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub footnote: Option<String>,
}

impl MigrationPlan {
    /// Compute the BLAKE3 hash over the canonical JSON serialization.
    /// YAML and JSON of the same plan produce the same hash — the hash identifies
    /// the plan's content, not its on-disk representation.
    pub fn canonical_hash(&self) -> String {
        let canonical = serde_json::to_string(self).expect("MigrationPlan must always serialize");
        blake3::hash(canonical.as_bytes()).to_hex().to_string()
    }
}

// ── typed op payloads ─────────────────────────────────────────────────────────
//
// The typed successor to indexing `MigrationOp.fields` at execution (ADR 0016 /
// NRN-405 part b). Each structural payload struct mirrors the keys of the
// `fields` object the on-disk plan carries; [`TypedOp::try_from`] reads those
// keys by hand with the applier's prior tolerance (unknown fields ignored — a
// repair-sourced op can carry harmless leftover keys). The structs are a
// construction-time view, never serialized — typing the vocabulary does NOT
// change the parity-pinned plan JSON.
//
// The frontmatter/body change ops and the section/body edit ops carry payloads
// that ARE `norn-core`'s own `PlannedChange` / `EditOp` models — types this crate
// may not name (the dependency rule). Their [`TypedOp`] variants therefore carry
// the raw `fields` object for the executor to deserialize into those core types;
// the untyped map does not flow further than that one typed decode.

use serde_json::{Map, Value};

/// `move_folder` payload: relocate a folder, cascading its documents.
#[derive(Debug, Clone, PartialEq)]
pub struct MoveFolderFields {
    pub src: String,
    pub dst: String,
    pub parents: bool,
}

/// `rewrite_wikilink` payload: rewrite every `[[old]]` reference to `[[new]]`.
#[derive(Debug, Clone, PartialEq)]
pub struct RewriteWikilinkFields {
    pub old: String,
    pub new: String,
}

/// `move_document` payload: relocate one document and (unless `no_link_rewrite`)
/// cascade its backlinks.
#[derive(Debug, Clone, PartialEq)]
pub struct MoveDocumentFields {
    pub src: String,
    pub dst: String,
    pub parents: bool,
    pub force: bool,
    pub no_link_rewrite: bool,
}

/// `delete_document` payload: remove a document, optionally redirecting its
/// incoming links to `rewrite_to`.
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteDocumentFields {
    pub path: String,
    pub rewrite_to: Option<String>,
}

/// A frontmatter/body change op (`set_frontmatter` / `add_frontmatter` /
/// `remove_frontmatter` / `rewrite_link` / `replace_body` / `create_document`).
/// The payload IS `norn-core`'s `PlannedChange` shape (unnameable here), so the
/// raw `fields` object rides through for the executor to deserialize.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangeOp {
    pub kind: String,
    pub fields: Map<String, Value>,
}

/// A section/body edit op (`str_replace` / `replace_section` /
/// `append_to_section` / `delete_section` / `insert_before_heading` /
/// `insert_after_heading`). The payload IS an `EditOp` (a `norn-core` type),
/// carried raw; `path`/`document_hash` are pre-extracted for the executor's
/// change envelope, and `id` supplies the change-id when present.
#[derive(Debug, Clone, PartialEq)]
pub struct EditOp {
    pub kind: String,
    pub id: Option<String>,
    pub path: String,
    pub document_hash: String,
    pub fields: Map<String, Value>,
}

/// The typed operation vocabulary the executor consumes — the resolved view of a
/// [`MigrationOp`] once its `kind` + `fields` are interpreted. Obtain it via
/// `TypedOp::try_from(&op)`. An unrecognized `kind` (or a structural op missing a
/// required field) yields a [`TypedOpError`]; the executor maps it to the same
/// coded, report-shaped refusal it produced before (the messages are preserved
/// verbatim), so the plan-parses-but-refused-at-execute contract is unchanged.
#[derive(Debug, Clone, PartialEq)]
pub enum TypedOp {
    MoveFolder(MoveFolderFields),
    RewriteWikilink(RewriteWikilinkFields),
    MoveDocument(MoveDocumentFields),
    DeleteDocument(DeleteDocumentFields),
    Change(ChangeOp),
    Edit(EditOp),
}

/// Why a [`MigrationOp`] could not resolve to a [`TypedOp`]. The `Display` strings
/// are the EXACT messages the executor previously produced from its ad-hoc
/// indexing, so a routed/refused apply reconstructs byte-identically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypedOpError {
    /// The op `kind` is not one the executor knows. Message:
    /// `unknown operation kind: {0}` (parity-pinned).
    UnknownKind(String),
    /// A structural op is missing a required string field. Message:
    /// `{kind} missing {field}`.
    MissingField { kind: String, field: &'static str },
    /// A change/edit op's `fields` is not a JSON object. Message:
    /// `op.fields for {kind} must be an object`.
    FieldsNotObject { kind: String },
    /// A change op's `fields.operation` disagrees with its `kind`. Left
    /// unrefused, a reviewed plan would silently dispatch as `fields.operation`
    /// — executing a different operation than its `kind` declares. Message:
    /// `op.fields.operation '{operation}' conflicts with op.kind '{kind}'`.
    OperationKindMismatch { kind: String, operation: String },
}

impl std::fmt::Display for TypedOpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypedOpError::UnknownKind(kind) => write!(f, "unknown operation kind: {kind}"),
            TypedOpError::MissingField { kind, field } => write!(f, "{kind} missing {field}"),
            TypedOpError::FieldsNotObject { kind } => {
                write!(f, "op.fields for {kind} must be an object")
            }
            TypedOpError::OperationKindMismatch { kind, operation } => {
                write!(
                    f,
                    "op.fields.operation '{operation}' conflicts with op.kind '{kind}'"
                )
            }
        }
    }
}

impl std::error::Error for TypedOpError {}

impl TryFrom<&MigrationOp> for TypedOp {
    type Error = TypedOpError;

    fn try_from(op: &MigrationOp) -> Result<Self, TypedOpError> {
        // Read a required string field with the executor's historical message.
        let str_field = |field: &'static str| -> Result<String, TypedOpError> {
            op.fields
                .get(field)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or(TypedOpError::MissingField {
                    kind: op.kind.clone(),
                    field,
                })
        };
        let bool_field = |field: &str| -> bool {
            op.fields
                .get(field)
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        let object = || -> Result<Map<String, Value>, TypedOpError> {
            op.fields
                .as_object()
                .cloned()
                .ok_or(TypedOpError::FieldsNotObject {
                    kind: op.kind.clone(),
                })
        };

        Ok(match op.kind.as_str() {
            "move_folder" => TypedOp::MoveFolder(MoveFolderFields {
                src: str_field("src")?,
                dst: str_field("dst")?,
                parents: bool_field("parents"),
            }),
            "rewrite_wikilink" => TypedOp::RewriteWikilink(RewriteWikilinkFields {
                old: str_field("old")?,
                new: str_field("new")?,
            }),
            "move_document" => TypedOp::MoveDocument(MoveDocumentFields {
                src: str_field("src")?,
                dst: str_field("dst")?,
                parents: bool_field("parents"),
                force: bool_field("force"),
                no_link_rewrite: bool_field("no_link_rewrite"),
            }),
            "delete_document" => TypedOp::DeleteDocument(DeleteDocumentFields {
                path: str_field("path")?,
                rewrite_to: op
                    .fields
                    .get("rewrite_to")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            }),
            "set_frontmatter" | "add_frontmatter" | "remove_frontmatter" | "rewrite_link"
            | "replace_body" | "create_document" => TypedOp::Change(ChangeOp {
                kind: op.kind.clone(),
                fields: object()?,
            }),
            "str_replace"
            | "replace_section"
            | "append_to_section"
            | "delete_section"
            | "insert_before_heading"
            | "insert_after_heading" => {
                // `path` is required (matches the executor's `{kind} missing path`).
                let path = str_field("path")?;
                let document_hash = op
                    .fields
                    .get("document_hash")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                TypedOp::Edit(EditOp {
                    kind: op.kind.clone(),
                    id: op.id.clone(),
                    path,
                    document_hash,
                    fields: object()?,
                })
            }
            other => return Err(TypedOpError::UnknownKind(other.to_string())),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_plan_round_trips_json() {
        let plan = MigrationPlan {
            schema_version: MIGRATION_PLAN_SCHEMA_VERSION,
            vault_root: "/abs/vault".into(),
            generator: None,
            generated_at: None,
            preconditions: vec![],
            operations: vec![MigrationOp {
                kind: "move_document".into(),
                id: None,
                requires: vec![],
                fields: serde_json::json!({"src": "a.md", "dst": "b.md"}),
                footnote: None,
            }],
            skipped: vec![],
            plan_footnote: None,
        };
        let json = serde_json::to_string(&plan).unwrap();
        let back: MigrationPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(back.schema_version, MIGRATION_PLAN_SCHEMA_VERSION);
        assert_eq!(back.operations.len(), 1);
        assert_eq!(back.operations[0].kind, "move_document");
    }

    #[test]
    fn migration_plan_round_trips_yaml() {
        let yaml = r#"
schema_version: 2
vault_root: /abs/vault
operations:
  - kind: move_folder
    fields:
      src: src_dir
      dst: dst_dir
"#;
        let plan: MigrationPlan = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(plan.operations[0].kind, "move_folder");
        let back = serde_yaml::to_string(&plan).unwrap();
        let parsed: MigrationPlan = serde_yaml::from_str(&back).unwrap();
        assert_eq!(parsed.operations[0].kind, "move_folder");
    }

    #[test]
    fn canonical_hash_matches_across_json_and_yaml() {
        // Same content via different formats hashes identically.
        let yaml = r#"
schema_version: 2
vault_root: /abs/vault
operations:
  - kind: move_document
    fields:
      src: a.md
      dst: b.md
"#;
        let from_yaml: MigrationPlan = serde_yaml::from_str(yaml).unwrap();
        let json = serde_json::to_string(&from_yaml).unwrap();
        let from_json: MigrationPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(from_yaml.canonical_hash(), from_json.canonical_hash());
    }

    #[test]
    fn canonical_hash_is_stable_across_field_key_order() {
        // Semantically identical plans with `fields` keys authored in different
        // orders hash identically — pins the map-canonicalization the hash
        // depends on (a future preserve_order opt-in must not change digests).
        let a: MigrationPlan = serde_yaml::from_str(
            "schema_version: 2\nvault_root: /abs/vault\noperations:\n  - kind: move_document\n    fields:\n      src: a.md\n      dst: b.md\n",
        )
        .unwrap();
        let b: MigrationPlan = serde_yaml::from_str(
            "schema_version: 2\nvault_root: /abs/vault\noperations:\n  - kind: move_document\n    fields:\n      dst: b.md\n      src: a.md\n",
        )
        .unwrap();
        assert_eq!(a.canonical_hash(), b.canonical_hash());
    }

    #[test]
    fn owner_set_precondition_round_trips_with_narrow_selector() {
        let json = serde_json::json!({
            "schema_version": 2,
            "vault_root": "/abs/vault",
            "preconditions": [{
                "id": "project-owner",
                "kind": "owner_set",
                "selector": {"eq": ["type:project", "key:MMR"]},
                "expected_paths": ["projects/mimir.md"]
            }],
            "operations": []
        });
        let plan: MigrationPlan = serde_json::from_value(json).unwrap();
        assert_eq!(plan.preconditions[0].id(), "project-owner");
        let serialized = serde_json::to_value(plan).unwrap();
        assert_eq!(
            serialized["preconditions"][0]["selector"]["eq"],
            serde_json::json!(["type:project", "key:MMR"])
        );
    }

    #[test]
    fn owner_selector_rejects_mixed_grammar() {
        let json = serde_json::json!({
            "schema_version": 2,
            "vault_root": "/abs/vault",
            "preconditions": [{
                "id": "ambiguous-owner",
                "kind": "owner_set",
                "selector": {"stem": "MMR", "eq": ["type:project"]},
                "expected_paths": []
            }],
            "operations": []
        });
        assert!(serde_json::from_value::<MigrationPlan>(json).is_err());
    }
}
