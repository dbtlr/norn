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

/// One typed operation. `fields` is an untyped JSON payload the applier
/// interprets per `kind`; typing the payloads is tracked separately (ADR 0016).
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
