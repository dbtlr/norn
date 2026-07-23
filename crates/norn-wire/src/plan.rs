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
/// JSON payload (an authored plan's `kind`+`fields` layout is a stable
/// on-disk/wire contract). At the EXECUTION boundary the executor resolves an op
/// to the typed [`TypedOp`] vocabulary via `TypedOp::try_from(&op)`, so untyped
/// `fields` indexing does not flow into the applier (NRN-405 part b / ADR 0016).
///
/// `kind` stays a free `String` (not a closed enum) on purpose: an UNRECOGNIZED
/// kind must still PARSE — it is refused later, at the [`TypedOp`] conversion, so
/// the "unknown kind is an executor refusal, not a deserialize rejection" contract
/// holds.
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
    /// Compute the BLAKE3 hash over the canonical JSON serialization of the
    /// plan's SEMANTIC content. YAML and JSON of the same plan produce the
    /// same hash — the hash identifies the plan's content, not its on-disk
    /// representation.
    ///
    /// Classification rule, enforced at compile time by the destructure below:
    /// a field that changes what the plan DOES is semantic and is hashed; a
    /// field that only records when/how the plan was produced, without
    /// changing its effect, is provenance and is excluded. A new
    /// `MigrationPlan` field breaks this destructure until it is named on one
    /// side or the other, so classification can't be forgotten by omission.
    ///
    /// - `generated_at` is EXCLUDED: a wall-clock provenance stamp (set by the
    ///   repair generator, `None` for hand-authored/verb-synthesized plans),
    ///   never semantic content, so two otherwise-identical plans stamped at
    ///   different instants must hash identically — the CAS/plan-identity
    ///   contract (ADR 0015/0024) depends on that determinism.
    /// - `generator` IS provenance too (it names the producer, not the plan's
    ///   effect) but is hashed anyway — an explicit, current contract choice,
    ///   not an oversight. It is a fixed literal per producer, so it doesn't
    ///   reintroduce non-determinism the way a timestamp would; changing this
    ///   to exclude `generator` would be a hash-contract change, not a bugfix.
    /// - Every other field is semantic plan content and is hashed.
    pub fn canonical_hash(&self) -> String {
        let MigrationPlan {
            schema_version,
            vault_root,
            generator,
            generated_at: _,
            preconditions,
            operations,
            skipped,
            plan_footnote,
        } = self;

        /// The hashed projection of a [`MigrationPlan`] — every field named
        /// explicitly, per the classification rule on [`MigrationPlan::canonical_hash`].
        #[derive(Serialize)]
        struct HashView<'a> {
            schema_version: &'a u32,
            vault_root: &'a String,
            #[serde(skip_serializing_if = "Option::is_none")]
            generator: &'a Option<String>,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            preconditions: &'a Vec<PlanPrecondition>,
            operations: &'a Vec<MigrationOp>,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            skipped: &'a Vec<SkippedFinding>,
            #[serde(skip_serializing_if = "Option::is_none")]
            plan_footnote: &'a Option<String>,
        }

        let view = HashView {
            schema_version,
            vault_root,
            generator,
            preconditions,
            operations,
            skipped,
            plan_footnote,
        };
        let canonical = serde_json::to_string(&view).expect("MigrationPlan must always serialize");
        blake3::hash(canonical.as_bytes()).to_hex().to_string()
    }
}

// ── typed op payloads ─────────────────────────────────────────────────────────
//
// The typed successor to indexing `MigrationOp.fields` at execution (ADR 0016 /
// NRN-405 part b, ADR 0022). Every op payload — the structural move/delete ops
// AND the frontmatter/body change and section/body edit ops — resolves to a real
// typed struct here; no `Map<String, Value>` payload rides on the plan path past
// [`TypedOp::try_from`]. The interior crates (`planner::intent`) consume the typed
// structs and adapt them into `norn-core`'s own `ApplyOp` at that boundary.
//
// The structs mirror the keys of the `fields` object the on-disk plan carries.
// They are a construction-time view, never serialized — typing the vocabulary does
// NOT change the on-disk plan JSON (`MigrationOp.fields` stays the raw
// `Value` envelope). `Value` survives ONLY at genuinely-arbitrary-JSON leaves
// (`ChangeFields::expected_old_value` / `new_value` — a frontmatter value is any
// JSON, exactly as `ApplyOp` types it), never as an opaque whole-op map.

use serde_json::{Map, Value};

/// `move_folder` payload: relocate a folder, cascading its documents.
///
/// `force` and `no_link_rewrite` carry the same semantics as the single-document
/// [`MoveDocumentFields`] flags and propagate to every expanded per-document op:
/// `force` overwrites an existing destination, `no_link_rewrite` suppresses the
/// backlink cascade. Both decode strictly (`bool_field`): absent/`null` → the
/// historical `false`, present-but-wrong-typed refuses.
#[derive(Debug, Clone, PartialEq)]
pub struct MoveFolderFields {
    pub src: String,
    pub dst: String,
    pub parents: bool,
    pub force: bool,
    pub no_link_rewrite: bool,
}

/// `rewrite_wikilink` payload: rewrite every `[[old]]` reference to `[[new]]`.
#[derive(Debug, Clone, PartialEq)]
pub struct RewriteWikilinkFields {
    pub old: String,
    pub new: String,
}

/// `move_document` payload: relocate one document and (unless `no_link_rewrite`)
/// cascade its backlinks.
///
/// `document_hash` is the OPTIONAL plan-time compare-and-swap precondition (ADR
/// 0024): stamped by the move verb / repair planner from the index at plan
/// synthesis, it drives a pre-rename fingerprint check against the file bytes; a
/// drifted source refuses `stale-document-hash`. Absent → no check (move stays
/// optional, unlike delete). `finding_code` / `repair_rule` ride as optional
/// finding-provenance (ADR 0022) so a repair-emitted structural op's linkage is
/// decoded and echoed through the typed path like a change op. All three decode
/// strictly: present-but-wrong-typed refuses, `null` == absent.
#[derive(Debug, Clone, PartialEq)]
pub struct MoveDocumentFields {
    pub src: String,
    pub dst: String,
    pub parents: bool,
    pub force: bool,
    pub no_link_rewrite: bool,
    pub document_hash: Option<String>,
    pub finding_code: Option<String>,
    pub repair_rule: Option<String>,
}

/// `delete_document` payload: remove a document, optionally redirecting its
/// incoming links to `rewrite_to`.
///
/// `document_hash` is the plan-time compare-and-swap precondition (ADR 0024).
/// Unlike move, a delete's hash is REQUIRED (NRN-151): the executor's plan-level
/// validation refuses a hash-less delete `delete-hash-required` before any write,
/// so a delete always CAS-checks the file bytes (`fingerprint_vacate`) before
/// removal. The verbs and repair always stamp it; only hand-authored hash-less
/// deletes newly refuse. `finding_code` / `repair_rule` ride as optional
/// finding-provenance (ADR 0022), echoed through the typed path like a change op.
/// All decode strictly: present-but-wrong-typed refuses, `null` == absent.
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteDocumentFields {
    pub path: String,
    pub rewrite_to: Option<String>,
    pub document_hash: Option<String>,
    pub finding_code: Option<String>,
    pub repair_rule: Option<String>,
}

/// The decoded `fields` of a frontmatter/body change op. Decoded strictly: a
/// wrong-typed member refuses (`MalformedFields`), never coerces. `path` is
/// required; every other member is optional and defaults at the
/// [`planner::intent`](../../norn_core/planner/intent/index.html) adapter boundary
/// where the interior apply op is built.
///
/// Finding linkage (`finding_code` / `repair_rule`) rides here as optional
/// provenance: authored ops omit it (decoding to `None`); repair-sourced ops carry
/// the real codes. Absence stays `None` end-to-end — nothing fabricates a
/// placeholder on either side of the wire. `expected_old_value` / `new_value`
/// stay `Value` — a frontmatter value is arbitrary JSON.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ChangeFields {
    pub path: String,
    #[serde(default)]
    pub change_id: Option<String>,
    #[serde(default)]
    pub document_hash: Option<String>,
    #[serde(default)]
    pub finding_code: Option<String>,
    #[serde(default)]
    pub finding_rule: Option<String>,
    #[serde(default)]
    pub repair_rule: Option<String>,
    /// The write-dispatch discriminator when present. Refused later if it
    /// disagrees with the op `kind`; filled from `kind` when absent.
    #[serde(default)]
    pub operation: Option<String>,
    #[serde(default)]
    pub field: Option<String>,
    #[serde(default)]
    pub expected_old_value: Option<Value>,
    #[serde(default)]
    pub new_value: Option<Value>,
    #[serde(default)]
    pub destination: Option<String>,
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub parents: bool,
}

/// A frontmatter/body change op (`set_frontmatter` / `add_frontmatter` /
/// `remove_frontmatter` / `rewrite_link` / `replace_body` / `create_document`)
/// with its [`ChangeFields`] payload decoded. `planner::intent` adapts the typed
/// payload into a `norn-core` `ApplyOp` at the execution boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangeOp {
    pub kind: String,
    pub fields: ChangeFields,
}

/// The decoded `fields` of a section/body edit op — the wire-owned mirror of the
/// authored subset of a plan's section-edit payload (a `norn-core` `EditOp` body
/// plus its addressing). `path` is pre-extracted (required) by
/// [`TypedOp::try_from`]; the remaining members are the union of the section-edit
/// op body fields, each decoded strictly so a wrong-typed member (a non-string
/// `document_hash`, a non-bool `replace_all`) refuses rather than coerces. The
/// interior adapter reassembles these into the `op`-tagged JSON the transform
/// engine deserializes into its own `EditOp`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EditFields {
    #[serde(default)]
    pub document_hash: Option<String>,
    #[serde(default)]
    pub old: Option<String>,
    #[serde(default)]
    pub new: Option<String>,
    #[serde(default)]
    pub replace_all: Option<bool>,
    #[serde(default)]
    pub heading: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
}

/// A section/body edit op (`str_replace` / `replace_section` /
/// `append_to_section` / `delete_section` / `insert_before_heading` /
/// `insert_after_heading`). `path` is pre-extracted for the executor's change
/// envelope, `id` supplies the change-id when present, and [`EditFields`] carries
/// the typed body.
#[derive(Debug, Clone, PartialEq)]
pub struct EditOp {
    pub kind: String,
    pub id: Option<String>,
    pub path: String,
    pub fields: EditFields,
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
/// are the exact messages the executor produces during indexing, so a
/// routed/refused apply reconstructs them identically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypedOpError {
    /// The op `kind` is not one the executor knows. Message:
    /// `unknown operation kind: {0}`.
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
    /// A change op's `fields` are the right SHAPE (a JSON object) but a member is
    /// wrong-TYPED, so the payload cannot decode into the executor's change model
    /// (e.g. `"operation": 5` or a non-bool `"force"`). Carries the underlying
    /// decode error text. Message:
    /// `op.fields for {kind} could not be decoded: {message}`.
    MalformedFields { kind: String, message: String },
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
            TypedOpError::MalformedFields { kind, message } => {
                write!(f, "op.fields for {kind} could not be decoded: {message}")
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
        // Strict boolean decode (ADR 0022): absent (or explicit `null`, the
        // verbs' "unset" sentinel) → the historical `false` default;
        // present-and-bool → its value; present-but-wrong-typed → refuse
        // (`"force": "true"` is a malformed plan, not a silent `false`).
        let bool_field = |field: &'static str| -> Result<bool, TypedOpError> {
            match op.fields.get(field) {
                None | Some(Value::Null) => Ok(false),
                Some(Value::Bool(b)) => Ok(*b),
                Some(_) => Err(TypedOpError::MalformedFields {
                    kind: op.kind.clone(),
                    message: format!("field `{field}` must be a boolean"),
                }),
            }
        };
        // Strict optional-string decode (ADR 0024), the sibling of `bool_field`:
        // absent or explicit `null` (the verbs' "unset" sentinel) → `None`;
        // present-and-string → `Some`; present-but-wrong-typed → refuse (never
        // coerce). The structural ops' `document_hash` / `finding_code` /
        // `repair_rule` — and `delete_document`'s `rewrite_to` — all decode through
        // this one arm so a non-string value is no longer indistinguishable from
        // absent.
        let opt_str_field = |field: &'static str| -> Result<Option<String>, TypedOpError> {
            match op.fields.get(field) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) => Ok(Some(s.clone())),
                Some(_) => Err(TypedOpError::MalformedFields {
                    kind: op.kind.clone(),
                    message: format!("field `{field}` must be a string"),
                }),
            }
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
                parents: bool_field("parents")?,
                force: bool_field("force")?,
                no_link_rewrite: bool_field("no_link_rewrite")?,
            }),
            "rewrite_wikilink" => TypedOp::RewriteWikilink(RewriteWikilinkFields {
                old: str_field("old")?,
                new: str_field("new")?,
            }),
            "move_document" => TypedOp::MoveDocument(MoveDocumentFields {
                src: str_field("src")?,
                dst: str_field("dst")?,
                parents: bool_field("parents")?,
                force: bool_field("force")?,
                no_link_rewrite: bool_field("no_link_rewrite")?,
                document_hash: opt_str_field("document_hash")?,
                finding_code: opt_str_field("finding_code")?,
                repair_rule: opt_str_field("repair_rule")?,
            }),
            "delete_document" => {
                let path = str_field("path")?;
                // Strict `rewrite_to` (ADR 0022): absent, or explicit `null` (the
                // delete verb's "no redirect" sentinel), → `None`; present-and-string
                // → `Some`; present-but-wrong-typed → refuse (a non-string, non-null
                // value is no longer indistinguishable from absent). The optional
                // hash + linkage decode the same way (ADR 0024).
                TypedOp::DeleteDocument(DeleteDocumentFields {
                    path,
                    rewrite_to: opt_str_field("rewrite_to")?,
                    document_hash: opt_str_field("document_hash")?,
                    finding_code: opt_str_field("finding_code")?,
                    repair_rule: opt_str_field("repair_rule")?,
                })
            }
            "set_frontmatter" | "add_frontmatter" | "remove_frontmatter" | "rewrite_link"
            | "replace_body" | "create_document" => {
                // Non-object `fields` refuses identically on every arm (FieldsNotObject
                // BEFORE any member decode).
                let obj = object()?;
                // Required `path` refuses `MissingField` like the edit and delete
                // arms (a non-string `path` is treated as missing, matching them),
                // rather than surfacing as serde's generic missing-field text.
                str_field("path")?;
                let fields: ChangeFields =
                    serde_json::from_value(Value::Object(obj)).map_err(|e| {
                        TypedOpError::MalformedFields {
                            kind: op.kind.clone(),
                            message: e.to_string(),
                        }
                    })?;
                TypedOp::Change(ChangeOp {
                    kind: op.kind.clone(),
                    fields,
                })
            }
            "str_replace"
            | "replace_section"
            | "append_to_section"
            | "delete_section"
            | "insert_before_heading"
            | "insert_after_heading" => {
                // FieldsNotObject BEFORE the required-field read, so a non-object
                // `fields` refuses `FieldsNotObject` here too (was misreported as
                // `missing path`).
                let obj = object()?;
                // `path` is required (matches the executor's `{kind} missing path`);
                // a non-string `path` is treated as missing, as before.
                let path = str_field("path")?;
                let fields: EditFields =
                    serde_json::from_value(Value::Object(obj)).map_err(|e| {
                        TypedOpError::MalformedFields {
                            kind: op.kind.clone(),
                            message: e.to_string(),
                        }
                    })?;
                TypedOp::Edit(EditOp {
                    kind: op.kind.clone(),
                    id: op.id.clone(),
                    path,
                    fields,
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
    fn canonical_hash_ignores_generated_at() {
        // Two plans identical in every field except `generated_at` (the
        // repair generator's wall-clock stamp, ADR 0024 / NRN-415) must hash
        // identically — the CAS/plan-identity contract cannot depend on when
        // the plan was generated.
        let base = MigrationPlan {
            schema_version: MIGRATION_PLAN_SCHEMA_VERSION,
            vault_root: "/abs/vault".into(),
            generator: Some("norn-repair".into()),
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
        let stamped_early = MigrationPlan {
            generated_at: Some("2026-01-01T00:00:00+00:00".into()),
            ..base.clone()
        };
        let stamped_late = MigrationPlan {
            generated_at: Some("2026-12-31T23:59:59+00:00".into()),
            ..base.clone()
        };
        assert_eq!(
            stamped_early.canonical_hash(),
            stamped_late.canonical_hash()
        );
        // Also matches the unstamped (`None`) plan the existing tests cover.
        assert_eq!(base.canonical_hash(), stamped_early.canonical_hash());
    }

    #[test]
    fn canonical_hash_differs_on_semantic_change() {
        // Closes the contract's other direction: two plans differing only in
        // one operation's `fields` (semantic content, unlike `generated_at`)
        // must hash DIFFERENTLY — the hash must not collapse a real content
        // change the way it deliberately collapses a provenance-only one.
        let base = MigrationPlan {
            schema_version: MIGRATION_PLAN_SCHEMA_VERSION,
            vault_root: "/abs/vault".into(),
            generator: Some("norn-repair".into()),
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
        let changed = MigrationPlan {
            operations: vec![MigrationOp {
                kind: "move_document".into(),
                id: None,
                requires: vec![],
                fields: serde_json::json!({"src": "a.md", "dst": "c.md"}),
                footnote: None,
            }],
            ..base.clone()
        };
        assert_ne!(base.canonical_hash(), changed.canonical_hash());
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

    // ── typed op payload decode (ADR 0022) ────────────────────────────────────

    fn op(kind: &str, fields: serde_json::Value) -> MigrationOp {
        MigrationOp {
            kind: kind.into(),
            id: None,
            requires: vec![],
            fields,
            footnote: None,
        }
    }

    #[test]
    fn move_document_decodes_bools_and_defaults_absent() {
        let o = op(
            "move_document",
            serde_json::json!({"src": "a.md", "dst": "b.md", "parents": true}),
        );
        let TypedOp::MoveDocument(f) = TypedOp::try_from(&o).unwrap() else {
            panic!("expected MoveDocument");
        };
        assert_eq!(f.src, "a.md");
        assert_eq!(f.dst, "b.md");
        assert!(f.parents);
        // `force` / `no_link_rewrite` absent → false (historical default).
        assert!(!f.force);
        assert!(!f.no_link_rewrite);
    }

    #[test]
    fn strict_bool_refuses_wrong_typed_force() {
        // `"force": "true"` (a string) previously coerced to a silent `false`;
        // it now refuses with a precise `MalformedFields` (ADR 0022 F5).
        let o = op(
            "move_document",
            serde_json::json!({"src": "a.md", "dst": "b.md", "force": "true"}),
        );
        let err = TypedOp::try_from(&o).unwrap_err();
        assert!(
            matches!(&err, TypedOpError::MalformedFields { kind, .. } if kind == "move_document"),
            "expected MalformedFields, got {err:?}"
        );
    }

    #[test]
    fn strict_bool_refuses_wrong_typed_parents_and_no_link_rewrite() {
        for field in ["parents", "no_link_rewrite"] {
            let mut fields = serde_json::json!({"src": "a.md", "dst": "b.md"});
            fields[field] = serde_json::json!(1); // a number, not a bool
            let err = TypedOp::try_from(&op("move_document", fields)).unwrap_err();
            assert!(
                matches!(&err, TypedOpError::MalformedFields { .. }),
                "expected MalformedFields for {field}, got {err:?}"
            );
        }
    }

    #[test]
    fn strict_bool_treats_null_as_absent_default() {
        // `null` is the verbs' "unset" sentinel — historically coerced to the
        // default, so it must NOT refuse.
        let o = op(
            "move_document",
            serde_json::json!({"src": "a.md", "dst": "b.md", "no_link_rewrite": null}),
        );
        let TypedOp::MoveDocument(f) = TypedOp::try_from(&o).unwrap() else {
            panic!("expected MoveDocument");
        };
        assert!(!f.no_link_rewrite);
    }

    #[test]
    fn delete_rewrite_to_string_null_and_wrong_typed() {
        // present-and-string → Some
        let TypedOp::DeleteDocument(f) = TypedOp::try_from(&op(
            "delete_document",
            serde_json::json!({"path": "b.md", "rewrite_to": "c.md"}),
        ))
        .unwrap() else {
            panic!("expected DeleteDocument");
        };
        assert_eq!(f.rewrite_to.as_deref(), Some("c.md"));

        // explicit null (the delete verb's no-redirect sentinel) → None
        let TypedOp::DeleteDocument(f) = TypedOp::try_from(&op(
            "delete_document",
            serde_json::json!({"path": "b.md", "rewrite_to": null}),
        ))
        .unwrap() else {
            panic!("expected DeleteDocument");
        };
        assert_eq!(f.rewrite_to, None);

        // present-but-wrong-typed (a number) → refuse, no longer silently `None`
        let err = TypedOp::try_from(&op(
            "delete_document",
            serde_json::json!({"path": "b.md", "rewrite_to": 5}),
        ))
        .unwrap_err();
        assert!(
            matches!(&err, TypedOpError::MalformedFields { kind, .. } if kind == "delete_document"),
            "expected MalformedFields, got {err:?}"
        );
    }

    #[test]
    fn edit_document_hash_absent_string_and_wrong_typed() {
        // absent → resolved to "" at the adapter boundary (fields carries None)
        let TypedOp::Edit(e) = TypedOp::try_from(&op(
            "str_replace",
            serde_json::json!({"path": "a.md", "old": "x", "new": "y"}),
        ))
        .unwrap() else {
            panic!("expected Edit");
        };
        assert_eq!(e.path, "a.md");
        assert_eq!(e.fields.document_hash, None);
        assert_eq!(e.fields.old.as_deref(), Some("x"));

        // present-and-string → Some
        let TypedOp::Edit(e) = TypedOp::try_from(&op(
            "str_replace",
            serde_json::json!({"path": "a.md", "old": "x", "new": "y", "document_hash": "deadbeef"}),
        ))
        .unwrap() else {
            panic!("expected Edit");
        };
        assert_eq!(e.fields.document_hash.as_deref(), Some("deadbeef"));

        // present-but-wrong-typed (a number) → refuse (was silently coerced to "")
        let err = TypedOp::try_from(&op(
            "str_replace",
            serde_json::json!({"path": "a.md", "old": "x", "new": "y", "document_hash": 5}),
        ))
        .unwrap_err();
        assert!(
            matches!(&err, TypedOpError::MalformedFields { kind, .. } if kind == "str_replace"),
            "expected MalformedFields, got {err:?}"
        );
    }

    #[test]
    fn non_object_fields_refuse_fields_not_object_on_every_arm() {
        // Change arm and edit arm both report FieldsNotObject for a non-object
        // `fields` — the edit arm previously misreported it as `missing path`.
        for kind in ["set_frontmatter", "str_replace"] {
            let err = TypedOp::try_from(&op(kind, serde_json::json!("not-an-object"))).unwrap_err();
            assert!(
                matches!(&err, TypedOpError::FieldsNotObject { kind: k } if k == kind),
                "expected FieldsNotObject for {kind}, got {err:?}"
            );
        }
    }

    #[test]
    fn edit_missing_path_is_missing_field_not_malformed() {
        // An object `fields` without `path` keeps the `{kind} missing path`
        // contract (MissingField), distinct from the non-object FieldsNotObject.
        let err = TypedOp::try_from(&op(
            "append_to_section",
            serde_json::json!({"heading": "Tasks", "content": "- x"}),
        ))
        .unwrap_err();
        assert_eq!(
            err,
            TypedOpError::MissingField {
                kind: "append_to_section".into(),
                field: "path"
            }
        );
    }

    #[test]
    fn change_missing_path_is_missing_field_not_malformed() {
        // The change arm keeps the same `{kind} missing path` contract as the
        // edit and delete arms — not serde's generic missing-field text wrapped
        // in MalformedFields. A non-string `path` is treated as missing, like
        // the edit arm.
        for fields in [
            serde_json::json!({"field": "status", "new_value": "done"}),
            serde_json::json!({"path": 5, "field": "status", "new_value": "done"}),
        ] {
            let err = TypedOp::try_from(&op("set_frontmatter", fields)).unwrap_err();
            assert_eq!(
                err,
                TypedOpError::MissingField {
                    kind: "set_frontmatter".into(),
                    field: "path"
                }
            );
        }
    }

    #[test]
    fn change_fields_carry_optional_finding_linkage() {
        // Repair-sourced ops carry real linkage; it decodes to typed Options.
        let TypedOp::Change(c) = TypedOp::try_from(&op(
            "rewrite_link",
            serde_json::json!({
                "path": "a.md",
                "finding_code": "link-target-missing",
                "repair_rule": "closest-match"
            }),
        ))
        .unwrap() else {
            panic!("expected Change");
        };
        assert_eq!(
            c.fields.finding_code.as_deref(),
            Some("link-target-missing")
        );
        assert_eq!(c.fields.repair_rule.as_deref(), Some("closest-match"));

        // Authored ops omit it → None (no `operator-request` fabricated on the wire).
        let TypedOp::Change(c) = TypedOp::try_from(&op(
            "set_frontmatter",
            serde_json::json!({"path": "a.md", "field": "title", "new_value": "T"}),
        ))
        .unwrap() else {
            panic!("expected Change");
        };
        assert_eq!(c.fields.finding_code, None);
        assert_eq!(c.fields.repair_rule, None);
    }

    // ── structural-op hash + linkage decode (ADR 0024) ────────────────────────

    #[test]
    fn move_document_hash_absent_string_null_and_wrong_typed() {
        // absent → None (move CAS optional)
        let TypedOp::MoveDocument(f) = TypedOp::try_from(&op(
            "move_document",
            serde_json::json!({"src": "a.md", "dst": "b.md"}),
        ))
        .unwrap() else {
            panic!("expected MoveDocument");
        };
        assert_eq!(f.document_hash, None);

        // present-and-string → Some
        let TypedOp::MoveDocument(f) = TypedOp::try_from(&op(
            "move_document",
            serde_json::json!({"src": "a.md", "dst": "b.md", "document_hash": "cafe"}),
        ))
        .unwrap() else {
            panic!("expected MoveDocument");
        };
        assert_eq!(f.document_hash.as_deref(), Some("cafe"));

        // explicit null (the verbs' unset sentinel) → None
        let TypedOp::MoveDocument(f) = TypedOp::try_from(&op(
            "move_document",
            serde_json::json!({"src": "a.md", "dst": "b.md", "document_hash": null}),
        ))
        .unwrap() else {
            panic!("expected MoveDocument");
        };
        assert_eq!(f.document_hash, None);

        // present-but-wrong-typed (a number) → refuse, never coerce
        let err = TypedOp::try_from(&op(
            "move_document",
            serde_json::json!({"src": "a.md", "dst": "b.md", "document_hash": 5}),
        ))
        .unwrap_err();
        assert!(
            matches!(&err, TypedOpError::MalformedFields { kind, .. } if kind == "move_document"),
            "expected MalformedFields, got {err:?}"
        );
    }

    #[test]
    fn delete_document_hash_absent_string_and_wrong_typed() {
        // absent → None (the required-hash refusal is a plan-level executor
        // barrier, NOT a decode rejection — a hash-less delete still DECODES).
        let TypedOp::DeleteDocument(f) =
            TypedOp::try_from(&op("delete_document", serde_json::json!({"path": "b.md"}))).unwrap()
        else {
            panic!("expected DeleteDocument");
        };
        assert_eq!(f.document_hash, None);

        // present-and-string → Some
        let TypedOp::DeleteDocument(f) = TypedOp::try_from(&op(
            "delete_document",
            serde_json::json!({"path": "b.md", "document_hash": "beef"}),
        ))
        .unwrap() else {
            panic!("expected DeleteDocument");
        };
        assert_eq!(f.document_hash.as_deref(), Some("beef"));

        // present-but-wrong-typed (a bool) → refuse
        let err = TypedOp::try_from(&op(
            "delete_document",
            serde_json::json!({"path": "b.md", "document_hash": true}),
        ))
        .unwrap_err();
        assert!(
            matches!(&err, TypedOpError::MalformedFields { kind, .. } if kind == "delete_document"),
            "expected MalformedFields, got {err:?}"
        );
    }

    #[test]
    fn structural_ops_carry_optional_finding_linkage() {
        // A repair-emitted move op carries real linkage; it decodes to typed Options.
        let TypedOp::MoveDocument(f) = TypedOp::try_from(&op(
            "move_document",
            serde_json::json!({
                "src": "a.md", "dst": "b.md",
                "finding_code": "frontmatter-required-field-missing",
                "repair_rule": "set-default"
            }),
        ))
        .unwrap() else {
            panic!("expected MoveDocument");
        };
        assert_eq!(
            f.finding_code.as_deref(),
            Some("frontmatter-required-field-missing")
        );
        assert_eq!(f.repair_rule.as_deref(), Some("set-default"));

        // A verb-synthesized / authored delete omits linkage → None (omitted, not
        // fabricated).
        let TypedOp::DeleteDocument(f) = TypedOp::try_from(&op(
            "delete_document",
            serde_json::json!({"path": "b.md", "document_hash": "beef"}),
        ))
        .unwrap() else {
            panic!("expected DeleteDocument");
        };
        assert_eq!(f.finding_code, None);
        assert_eq!(f.repair_rule, None);

        // A wrong-typed linkage member refuses like any other strict field.
        let err = TypedOp::try_from(&op(
            "move_document",
            serde_json::json!({"src": "a.md", "dst": "b.md", "finding_code": 7}),
        ))
        .unwrap_err();
        assert!(
            matches!(&err, TypedOpError::MalformedFields { kind, .. } if kind == "move_document"),
            "expected MalformedFields, got {err:?}"
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
