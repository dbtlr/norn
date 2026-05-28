//! Expand a high-level `rewrite_wikilink` op into body-rewrite ops + frontmatter-update ops.
//!
//! Graph-aware: `classify_link_risk` identifies body matches (stem-only +
//! path-qualified + alias-resolved). A frontmatter walker identifies fields
//! whose value is the old wikilink target and emits `set_frontmatter` ops.
//!
//! Pre-flight: refuses if `old` does not resolve to any document in the vault.
//!
//! # Frontmatter walk (v1)
//!
//! Walks all string-valued frontmatter fields and replaces any value exactly
//! matching `[[<op.old>]]` (or whose string content equals `op.old` when
//! already a bare wikilink target).  This is a v1 text-match approach that
//! captures the primary dogfood case (workspace-typed fields that hold
//! `"[[target]]"`).
//!
//! TODO(plan-task-followup): schema-aware frontmatter walk — consult the
//! compiled `VaultConfig.validate.rules[*].field_types` map to restrict the
//! sweep to fields declared as `wikilink` or `wikilink_or_list`.  The current
//! approach is safe because a string value exactly matching `[[old]]` is
//! unlikely to be a false positive in practice, but the schema-aware version
//! is strictly correct.

use crate::core::{GraphIndex, LinkKind, LinkSourceArea};
use crate::standards::PlannedChange;
use anyhow::{anyhow, Result};
use camino::Utf8PathBuf;
use serde_json::Value;

#[allow(dead_code)] // wired by expand() dispatcher in Plan Task 6; called by applier in Plan Task 7
pub(crate) struct RewriteWikilinkOp {
    pub old: String,
    pub new: String,
}

/// Expand a `rewrite_wikilink` op into concrete body-rewrite and
/// frontmatter-update ops, one per affected link or frontmatter field.
///
/// Steps:
/// 1. Resolve `op.old` to a document path via stem-match in the index.
/// 2. Refuse (Err) if no document resolves — caller maps this to exit code 2.
/// 3. Walk every document's body links (source_context.area == Body) that
///    resolve to `old_path`; emit a `rewrite_link` PlannedChange per match.
/// 4. Walk every document's frontmatter for string values equal to
///    `[[<op.old>]]` and emit a `set_frontmatter` op for each match.
/// 5. Return the combined list.
///
/// Note: frontmatter wikilinks are parsed into `doc.links` with
/// `source_context.area == Frontmatter`.  We intentionally skip them in step 3
/// and handle them in step 4 via the frontmatter sweep.  This prevents
/// duplicating the same reference as both a `rewrite_link` and a
/// `set_frontmatter` op.
#[allow(dead_code)] // wired by expand() dispatcher in Plan Task 6; called by applier in Plan Task 7
pub(crate) fn expand_rewrite_wikilink(
    op: &RewriteWikilinkOp,
    index: &GraphIndex,
) -> Result<Vec<PlannedChange>> {
    // --- Step 1: resolve op.old → document path ---
    let old_path: Utf8PathBuf = resolve_stem_to_path(&op.old, index)
        .ok_or_else(|| anyhow!("rewrite_wikilink: '{}' does not resolve to any document in the vault (pre-flight refusal)", op.old))?;

    let mut changes: Vec<PlannedChange> = Vec::new();

    // --- Step 3: body-link rewrites ---
    // Walk all documents; for each body link (not frontmatter) that resolves to
    // old_path, emit a rewrite_link op.  We use the link's bare target as the
    // `expected_old_value` (what apply_rewrite_link matches inside `[[...]]`),
    // and op.new as `new_value`.
    for doc in &index.documents {
        for link in &doc.links {
            // Skip frontmatter-sourced links; those are handled in step 4.
            let is_body = link
                .source_context
                .as_ref()
                .map(|ctx| ctx.area == LinkSourceArea::Body)
                .unwrap_or(true); // links without context default to body treatment
            if !is_body {
                continue;
            }

            // Only wikilinks and embeds are rewritten here.  Markdown links
            // that happen to resolve to old_path are a different kind of
            // problem (they use relative paths, not stem names) and are not
            // in scope for rewrite_wikilink.
            if !matches!(link.kind, LinkKind::Wikilink | LinkKind::Embed) {
                continue;
            }

            // Check if this link resolves to the old target path.
            let resolves_to_old = link
                .resolved_path
                .as_ref()
                .map(|p| p == &old_path)
                .unwrap_or(false);
            if !resolves_to_old {
                continue;
            }

            // The bare target is what apply_rewrite_link matches against inside [[...]].
            // For stem-only links: link.target == "target"
            // For path-qualified links: link.target == "dir/target"
            // Both are matched by expected_old_value against the bare target text.
            let old_target = link.target.clone();

            changes.push(PlannedChange {
                change_id: format!("rewrite-wikilink-{}-{}", doc.path, old_target),
                path: doc.path.clone(),
                document_hash: doc.hash.clone(),
                finding_code: "operator-request".into(),
                finding_rule: None,
                repair_rule: "operator-request".into(),
                operation: "rewrite_link".into(),
                field: None,
                expected_old_value: Some(Value::String(old_target)),
                new_value: Some(Value::String(op.new.clone())),
                destination: None,
                link_risk: None,
                warnings: Vec::new(),
                force: false,
                parents: false,
            });
        }
    }

    // --- Step 4: frontmatter wikilink-valued field sweep ---
    // v1: text-match on string fields whose value is exactly "[[<op.old>]]".
    // TODO(plan-task-followup): schema-aware frontmatter walk using
    // VaultConfig.validate.rules[*].field_types to restrict to wikilink/wikilink_or_list fields.
    let old_wikilink_literal = format!("[[{}]]", op.old);
    let new_wikilink_literal = format!("[[{}]]", op.new);

    for doc in &index.documents {
        let Some(fm) = &doc.frontmatter else { continue };
        let Some(obj) = fm.as_object() else { continue };

        for (field_name, field_value) in obj {
            if let Some(s) = field_value.as_str() {
                if s == old_wikilink_literal {
                    changes.push(PlannedChange {
                        change_id: format!("rewrite-wikilink-fm-{}-{}", doc.path, field_name),
                        path: doc.path.clone(),
                        document_hash: doc.hash.clone(),
                        finding_code: "operator-request".into(),
                        finding_rule: None,
                        repair_rule: "operator-request".into(),
                        operation: "set_frontmatter".into(),
                        field: Some(field_name.clone()),
                        expected_old_value: Some(Value::String(old_wikilink_literal.clone())),
                        new_value: Some(Value::String(new_wikilink_literal.clone())),
                        destination: None,
                        link_risk: None,
                        warnings: Vec::new(),
                        force: false,
                        parents: false,
                    });
                }
            }
            // Also handle wikilink_or_list: array of wikilink strings.
            if let Some(arr) = field_value.as_array() {
                let new_arr: Vec<Value> = arr
                    .iter()
                    .map(|v| {
                        if v.as_str() == Some(old_wikilink_literal.as_str()) {
                            Value::String(new_wikilink_literal.clone())
                        } else {
                            v.clone()
                        }
                    })
                    .collect();
                if new_arr != *arr {
                    changes.push(PlannedChange {
                        change_id: format!("rewrite-wikilink-fm-arr-{}-{}", doc.path, field_name),
                        path: doc.path.clone(),
                        document_hash: doc.hash.clone(),
                        finding_code: "operator-request".into(),
                        finding_rule: None,
                        repair_rule: "operator-request".into(),
                        operation: "set_frontmatter".into(),
                        field: Some(field_name.clone()),
                        expected_old_value: Some(field_value.clone()),
                        new_value: Some(Value::Array(new_arr)),
                        destination: None,
                        link_risk: None,
                        warnings: Vec::new(),
                        force: false,
                        parents: false,
                    });
                }
            }
        }
    }

    Ok(changes)
}

/// Resolve a wikilink stem (or path-qualified target) to a document path in the
/// index.  Matches against `doc.stem` (case-insensitive), which mirrors the
/// resolution logic in `links/resolve.rs`.
#[allow(dead_code)] // called by expand_rewrite_wikilink which is wired by Plan Task 7
fn resolve_stem_to_path(stem: &str, index: &GraphIndex) -> Option<Utf8PathBuf> {
    let lower = stem.to_lowercase();
    // Try exact stem match first (most common).
    if let Some(doc) = index
        .documents
        .iter()
        .find(|d| d.stem.to_lowercase() == lower)
    {
        return Some(doc.path.clone());
    }
    // Try path-qualified match (stem may include directory prefix).
    if stem.contains('/') {
        let with_ext = format!("{stem}.md");
        if let Some(doc) = index
            .documents
            .iter()
            .find(|d| d.path.as_str().to_lowercase() == with_ext.to_lowercase())
        {
            return Some(doc.path.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn synth_vault_with_links() -> TempDir {
        let tmp = tempfile::Builder::new()
            .prefix("planner-rewrite-wikilink-")
            .tempdir()
            .unwrap();
        let root = tmp.path();
        // Target doc:
        std::fs::write(root.join("target.md"), "---\ntype: note\n---\n# Target\n").unwrap();
        // Body wikilink doc (stem-only):
        std::fs::write(
            root.join("a.md"),
            "---\ntype: note\n---\n# A\nReferences [[target]] inline.\n",
        )
        .unwrap();
        // Body wikilink doc (with display alias):
        std::fs::write(
            root.join("b.md"),
            "---\ntype: note\n---\n# B\nSee [[target|the target]] for details.\n",
        )
        .unwrap();
        // Frontmatter wikilink doc (workspace field):
        std::fs::write(
            root.join("c.md"),
            "---\ntype: note\nworkspace: \"[[target]]\"\n---\n# C\n",
        )
        .unwrap();
        // Distractor doc (no reference):
        std::fs::write(
            root.join("d.md"),
            "---\ntype: note\n---\n# D\nNothing here.\n",
        )
        .unwrap();
        tmp
    }

    #[test]
    fn expand_rewrite_wikilink_produces_body_and_frontmatter_ops() {
        let tmp = synth_vault_with_links();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let index = crate::graph::build_index(root).unwrap();

        let op = RewriteWikilinkOp {
            old: "target".into(),
            new: "new-target".into(),
        };
        let expanded = expand_rewrite_wikilink(&op, &index).unwrap();
        let body_rewrites: Vec<_> = expanded
            .iter()
            .filter(|c| c.operation == "rewrite_link")
            .collect();
        let fm_updates: Vec<_> = expanded
            .iter()
            .filter(|c| c.operation == "set_frontmatter")
            .collect();
        assert_eq!(
            body_rewrites.len(),
            2,
            "a.md + b.md → 2 rewrite_link ops; got {:?}",
            body_rewrites
                .iter()
                .map(|c| c.path.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            fm_updates.len(),
            1,
            "c.md workspace frontmatter → 1 set_frontmatter op; got {:?}",
            fm_updates
                .iter()
                .map(|c| format!("{}:{}", c.path, c.field.as_deref().unwrap_or("")))
                .collect::<Vec<_>>()
        );
        // Verify the frontmatter op points at c.md and the workspace field.
        let fm_op = &fm_updates[0];
        assert_eq!(fm_op.path.as_str(), "c.md");
        assert_eq!(fm_op.field.as_deref(), Some("workspace"));
        assert_eq!(
            fm_op.new_value.as_ref().and_then(|v| v.as_str()),
            Some("[[new-target]]")
        );
    }

    #[test]
    fn expand_rewrite_wikilink_refuses_when_old_unresolvable() {
        let tmp = synth_vault_with_links();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let index = crate::graph::build_index(root).unwrap();

        let op = RewriteWikilinkOp {
            old: "no-such-target".into(),
            new: "new-target".into(),
        };
        let result = expand_rewrite_wikilink(&op, &index);
        assert!(
            result.is_err(),
            "pre-flight should refuse when old target resolves to no document"
        );
    }

    #[test]
    fn expand_rewrite_wikilink_body_ops_have_correct_old_and_new_values() {
        let tmp = synth_vault_with_links();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let index = crate::graph::build_index(root).unwrap();

        let op = RewriteWikilinkOp {
            old: "target".into(),
            new: "new-target".into(),
        };
        let expanded = expand_rewrite_wikilink(&op, &index).unwrap();
        for change in expanded.iter().filter(|c| c.operation == "rewrite_link") {
            assert_eq!(
                change.new_value.as_ref().and_then(|v| v.as_str()),
                Some("new-target"),
                "new_value must be 'new-target' for rewrite_link ops"
            );
        }
    }
}
