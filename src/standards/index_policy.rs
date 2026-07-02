//! Resolved index-field set for the Wave 2 derived frontmatter index (EAV
//! narrow table). Pure config→policy function: no cache/SQL here — later
//! tasks (cache writer, query router) consume `resolved_index_set` to decide
//! which frontmatter fields get an indexed row.

// `resolved_index_set` has no production caller yet — this task is config
// surface only. The cache writer and query router tasks wire it in.
#![allow(dead_code)]

use std::collections::{BTreeSet, HashMap};

use sha2::{Digest, Sha256};

use crate::standards::config::VaultConfig;

/// Bounded field_types that qualify a field for automatic indexing (when
/// `index.auto` is true). `text` is deliberately excluded — it is the one
/// unbounded scalar type and never auto-qualifies.
fn is_auto_qualifying_type(type_name: &str) -> bool {
    matches!(
        type_name,
        "date" | "datetime" | "wikilink" | "wikilink_or_list" | "string" | "list_of_strings"
    )
}

#[derive(Default)]
struct FieldVote {
    explicit_true: bool,
    explicit_false: bool,
    auto_qualifies: bool,
}

/// Resolve the set of frontmatter fields to index, plus a stable hash of
/// that set (SHA-256 over the sorted, newline-joined field names).
///
/// Precedence per field, across every rule that mentions it:
/// 1. Any explicit `indexed: true` wins over everything.
/// 2. Else any explicit `indexed: false` wins over auto-qualification.
/// 3. Else auto-qualification applies (only when `index.auto` is true).
///
/// A field auto-qualifies when some rule gives it an `allowed_values`
/// constraint, or a bounded `field_types` type (`date`, `datetime`,
/// `wikilink`, `wikilink_or_list`, `string`, `list_of_strings`). `text` and
/// undeclared fields never auto-qualify.
pub(crate) fn resolved_index_set(cfg: &VaultConfig) -> (BTreeSet<String>, String) {
    let mut votes: HashMap<String, FieldVote> = HashMap::new();

    for rule in &cfg.validate.rules {
        for field in rule.allowed_values.keys() {
            votes.entry(field.clone()).or_default().auto_qualifies = true;
        }

        for (field, spec) in &rule.field_types {
            let vote = votes.entry(field.clone()).or_default();
            match spec.indexed() {
                Some(true) => vote.explicit_true = true,
                Some(false) => vote.explicit_false = true,
                None => {}
            }
            if spec.type_name().is_some_and(is_auto_qualifying_type) {
                vote.auto_qualifies = true;
            }
        }
    }

    let auto = cfg.index.auto;
    let set: BTreeSet<String> = votes
        .into_iter()
        .filter_map(|(field, vote)| {
            let included = if vote.explicit_true {
                true
            } else if vote.explicit_false {
                false
            } else {
                auto && vote.auto_qualifies
            };
            included.then_some(field)
        })
        .collect();

    let hash = hash_field_set(&set);
    (set, hash)
}

fn hash_field_set(fields: &BTreeSet<String>) -> String {
    let joined = fields.iter().cloned().collect::<Vec<_>>().join("\n");
    let mut hasher = Sha256::new();
    hasher.update(joined.as_bytes());
    crate::cache::hex_lower(hasher.finalize().as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8Path;

    fn parse(yaml: &str) -> VaultConfig {
        crate::standards::parse_config(yaml, Utf8Path::new(".norn/config.yaml"))
            .expect("config should parse")
    }

    #[test]
    fn empty_config_yields_empty_set() {
        let cfg = parse("");
        let (set, hash) = resolved_index_set(&cfg);
        assert!(set.is_empty());
        // Deterministic: hashing an empty field list twice yields the same hash.
        let (_, hash2) = resolved_index_set(&cfg);
        assert_eq!(hash, hash2);
    }

    #[test]
    fn allowed_values_field_auto_qualifies() {
        let cfg = parse(
            "validate:\n  rules:\n    - name: r\n      allowed_values:\n        status:\n          - a\n",
        );
        let (set, _) = resolved_index_set(&cfg);
        assert!(set.contains("status"));
    }

    #[test]
    fn bounded_field_types_auto_qualify() {
        let cfg = parse(
            r#"
validate:
  rules:
    - name: r
      field_types:
        created: date
        updated: datetime
        parent: wikilink
        related: wikilink_or_list
        project: string
        tags: list_of_strings
"#,
        );
        let (set, _) = resolved_index_set(&cfg);
        for field in ["created", "updated", "parent", "related", "project", "tags"] {
            assert!(set.contains(field), "{field} should auto-qualify");
        }
    }

    #[test]
    fn text_type_does_not_auto_qualify() {
        let cfg =
            parse("validate:\n  rules:\n    - name: r\n      field_types:\n        notes: text\n");
        let (set, _) = resolved_index_set(&cfg);
        assert!(!set.contains("notes"));
    }

    #[test]
    fn undeclared_field_never_qualifies() {
        let cfg = parse("");
        let (set, _) = resolved_index_set(&cfg);
        assert!(!set.contains("anything"));
    }

    #[test]
    fn explicit_indexed_true_includes_text_field() {
        let cfg = parse(
            "validate:\n  rules:\n    - name: r\n      field_types:\n        notes: { type: text, indexed: true }\n",
        );
        let (set, _) = resolved_index_set(&cfg);
        assert!(set.contains("notes"));
    }

    #[test]
    fn explicit_indexed_false_suppresses_auto_qualification() {
        let cfg = parse(
            "validate:\n  rules:\n    - name: r\n      field_types:\n        project: { type: string, indexed: false }\n",
        );
        let (set, _) = resolved_index_set(&cfg);
        assert!(!set.contains("project"));
    }

    #[test]
    fn auto_false_excludes_bounded_types_without_explicit_indexed() {
        let cfg = parse(
            "index:\n  auto: false\nvalidate:\n  rules:\n    - name: r\n      field_types:\n        created: date\n",
        );
        let (set, _) = resolved_index_set(&cfg);
        assert!(!set.contains("created"));
    }

    #[test]
    fn auto_false_still_includes_explicit_indexed_true() {
        let cfg = parse(
            "index:\n  auto: false\nvalidate:\n  rules:\n    - name: r\n      field_types:\n        notes: { type: text, indexed: true }\n",
        );
        let (set, _) = resolved_index_set(&cfg);
        assert!(set.contains("notes"));
    }

    #[test]
    fn auto_false_still_excludes_explicit_indexed_false() {
        let cfg = parse(
            "index:\n  auto: false\nvalidate:\n  rules:\n    - name: r\n      field_types:\n        project: { type: string, indexed: false }\n",
        );
        let (set, _) = resolved_index_set(&cfg);
        assert!(!set.contains("project"));
    }

    #[test]
    fn explicit_true_wins_over_explicit_false_across_rules() {
        let cfg = parse(
            r#"
validate:
  rules:
    - name: a
      field_types:
        notes: { type: text, indexed: true }
    - name: b
      field_types:
        notes: { type: text, indexed: false }
"#,
        );
        let (set, _) = resolved_index_set(&cfg);
        assert!(set.contains("notes"));
    }

    #[test]
    fn explicit_false_wins_over_auto_qualification_across_rules() {
        let cfg = parse(
            r#"
validate:
  rules:
    - name: a
      field_types:
        project: string
    - name: b
      field_types:
        project: { type: string, indexed: false }
"#,
        );
        let (set, _) = resolved_index_set(&cfg);
        assert!(!set.contains("project"));
    }

    #[test]
    fn multi_rule_union_of_qualifying_fields() {
        let cfg = parse(
            r#"
validate:
  rules:
    - name: a
      allowed_values:
        status:
          - x
    - name: b
      field_types:
        created: datetime
"#,
        );
        let (set, _) = resolved_index_set(&cfg);
        assert_eq!(
            set,
            BTreeSet::from(["status".to_string(), "created".to_string()])
        );
    }

    #[test]
    fn hash_changes_when_field_set_changes() {
        let cfg_a = parse(
            "validate:\n  rules:\n    - name: r\n      field_types:\n        created: date\n",
        );
        let cfg_b = parse(
            "validate:\n  rules:\n    - name: r\n      field_types:\n        updated: date\n",
        );
        let (_, hash_a) = resolved_index_set(&cfg_a);
        let (_, hash_b) = resolved_index_set(&cfg_b);
        assert_ne!(hash_a, hash_b);
    }

    #[test]
    fn hash_is_stable_regardless_of_rule_declaration_order() {
        let cfg_a = parse(
            r#"
validate:
  rules:
    - name: a
      field_types:
        created: date
    - name: b
      field_types:
        updated: date
"#,
        );
        let cfg_b = parse(
            r#"
validate:
  rules:
    - name: b
      field_types:
        updated: date
    - name: a
      field_types:
        created: date
"#,
        );
        let (set_a, hash_a) = resolved_index_set(&cfg_a);
        let (set_b, hash_b) = resolved_index_set(&cfg_b);
        assert_eq!(set_a, set_b);
        assert_eq!(hash_a, hash_b);
    }

    #[test]
    fn allowed_values_field_with_typeless_indexed_false_is_excluded() {
        let cfg = parse(
            r#"
validate:
  rules:
    - name: r
      allowed_values:
        status:
          - a
      field_types:
        status: { indexed: false }
"#,
        );
        let (set, _) = resolved_index_set(&cfg);
        assert!(!set.contains("status"));
    }

    #[test]
    fn plain_untyped_field_with_typeless_indexed_true_is_included() {
        let cfg = parse(
            "validate:\n  rules:\n    - name: r\n      field_types:\n        notes: { indexed: true }\n",
        );
        let (set, _) = resolved_index_set(&cfg);
        assert!(set.contains("notes"));
    }

    #[test]
    fn hash_is_hex_sha256_shape() {
        let cfg = parse(
            "validate:\n  rules:\n    - name: r\n      field_types:\n        created: date\n",
        );
        let (_, hash) = resolved_index_set(&cfg);
        assert_eq!(hash.len(), 64);
        assert!(hash
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }
}
