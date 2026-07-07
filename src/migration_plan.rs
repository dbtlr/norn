//! MigrationPlan — the unified on-disk artifact for apply + repair.
//!
//! Replaces `RepairPlan`. Schema starts at v1.

use serde::{Deserialize, Serialize};

pub const MIGRATION_PLAN_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationPlan {
    pub schema_version: u32,
    pub vault_root: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub generator: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub generated_at: Option<String>,
    pub operations: Vec<MigrationOp>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub skipped: Vec<SkippedFinding>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub plan_footnote: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
            schema_version: 1,
            vault_root: "/abs/vault".into(),
            generator: None,
            generated_at: None,
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
        assert_eq!(back.schema_version, 1);
        assert_eq!(back.operations.len(), 1);
        assert_eq!(back.operations[0].kind, "move_document");
    }

    #[test]
    fn migration_plan_round_trips_yaml() {
        let yaml = r#"
schema_version: 1
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
        // Self-review gap addressed: same content via different formats hashes identically.
        let yaml = r#"
schema_version: 1
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
}
