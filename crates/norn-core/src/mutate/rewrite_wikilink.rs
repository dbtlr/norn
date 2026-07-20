//! The `rewrite-wikilink` execute seam: rewrite every `[[old]]` reference (body
//! + frontmatter) to `[[new]]` across the vault.
//!
//! Ported from the donor `rewrite_wikilink::{mod, route}` (ADR 0018). A one-op
//! `rewrite_wikilink` `MigrationPlan` runs through the shared executor: the
//! planner expander resolves `old` against the graph and expands to per-document
//! `rewrite_link` / `set_frontmatter` ops, or REFUSES (unresolvable `old`) — a
//! refusal the executor carries out as a coded `outcome = refused` [`ApplyReport`]
//! under `refuse_as_report`, so no explicit preflight is needed here.

use super::{owner_index_options, MutationExecution};
use crate::apply::report::ApplyReport;
use crate::apply::{apply_migration_plan, ApplyContext};
use crate::plan::{MigrationOp, MigrationPlan, MIGRATION_PLAN_SCHEMA_VERSION};
use serde_json::json;

/// Execute a `rewrite-wikilink`: forecast (`confirm == false`) or apply.
pub fn execute(
    cache: &crate::cache::Cache,
    config: Option<&crate::standards::VaultConfig>,
    params: &norn_wire::RewriteWikilinkParams,
    _today: &str,
    sink: &mut crate::telemetry::EventSink,
) -> anyhow::Result<MutationExecution<ApplyReport>> {
    let index = cache.load_graph_index()?;
    let vault_root = cache.vault_root().to_string();
    let dry_run = !params.confirm;

    let op = MigrationOp {
        kind: "rewrite_wikilink".into(),
        id: None,
        requires: Vec::new(),
        fields: json!({ "old": params.old, "new": params.new }),
        footnote: None,
    };
    let plan = MigrationPlan {
        schema_version: MIGRATION_PLAN_SCHEMA_VERSION,
        vault_root,
        generator: None,
        generated_at: None,
        preconditions: Vec::new(),
        operations: vec![op],
        skipped: Vec::new(),
        plan_footnote: None,
    };

    let ctx = ApplyContext {
        dry_run,
        parents: false,
        verbose: false,
        refuse_as_report: true,
        owner_index_options: owner_index_options(config),
    };
    let apply_report = apply_migration_plan(&plan, &index, ctx, sink)?;
    // A forecast commits nothing (matches set/new).
    let touched_paths = if params.confirm {
        apply_report.touched_paths.clone()
    } else {
        Vec::new()
    };
    Ok(MutationExecution {
        report: apply_report,
        touched_paths,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::report::ApplyOutcome;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    const TODAY: &str = "2026-07-20";

    fn sink() -> crate::telemetry::EventSink {
        crate::telemetry::EventSink::discard(
            crate::telemetry::IdGen::with_seed(0),
            crate::telemetry::Clock::fixed("2026-07-20T00:00:00.000Z"),
        )
    }

    fn synth_vault(docs: &[(&str, &str)]) -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::create_dir(root.join(".norn").as_std_path()).unwrap();
        std::fs::write(
            root.join(".norn/config.yaml").as_std_path(),
            "validate: {}\n",
        )
        .unwrap();
        for (path, contents) in docs {
            let full = root.join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent.as_std_path()).unwrap();
            }
            std::fs::write(full.as_std_path(), contents).unwrap();
        }
        (tmp, root)
    }

    fn built(root: &Utf8PathBuf) -> crate::cache::Cache {
        let mut cache = crate::cache::Cache::open(root).unwrap();
        cache.full_build(root).unwrap();
        cache
    }

    fn params(old: &str, new: &str, confirm: bool) -> norn_wire::RewriteWikilinkParams {
        norn_wire::RewriteWikilinkParams {
            old: old.into(),
            new: new.into(),
            confirm,
        }
    }

    #[test]
    fn apply_rewrites_backlink() {
        let (_t, root) = synth_vault(&[
            ("a.md", "---\ntype: note\n---\n# A\n[[b]]\n"),
            ("b.md", "---\ntype: note\n---\n# B\n"),
            ("c.md", "---\ntype: note\n---\n# C\n"),
        ]);
        let cache = built(&root);
        let exec = execute(&cache, None, &params("b", "c", true), TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, ApplyOutcome::Applied);
        let a = std::fs::read_to_string(root.join("a.md").as_std_path()).unwrap();
        assert!(a.contains("[[c]]"), "wikilink rewritten: {a}");
        assert!(!exec.touched_paths.is_empty());
    }

    #[test]
    fn unresolvable_old_refuses() {
        let (_t, root) = synth_vault(&[("a.md", "---\ntype: note\n---\n# A\n")]);
        let cache = built(&root);
        let exec = execute(
            &cache,
            None,
            &params("no-such", "c", true),
            TODAY,
            &mut sink(),
        )
        .unwrap();
        assert_eq!(exec.report.outcome, ApplyOutcome::Refused);
        assert!(exec.touched_paths.is_empty());
    }
}
