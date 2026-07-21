//! The `apply` execute seam: execute an already-reviewed `MigrationPlan`.
//!
//! Ported from the donor `apply::{run_direct, route}` (ADR 0018). Unlike the
//! other cascade verbs, `apply` does NOT synthesize a plan from a handful of
//! arguments — the CLI has already read the plan source (file or stdin), detected
//! its format, parsed it into a `MigrationPlan`, and validated its
//! `schema_version` in the client-side preamble (a malformed plan or a schema
//! mismatch refuses BEFORE the wire, byte-identical to the donor). The parsed plan
//! crosses opaque in [`norn_wire::ApplyParams::plan`] (this seam's crate cannot be
//! named by `norn-wire`); here the owner deserializes it back and hands it to the
//! shared [`apply_migration_plan`] executor under `refuse_as_report` — so a clean
//! pre-write decline (an owner-set precondition mismatch, a containment violation,
//! a bad create path) returns a coded `outcome = refused` [`ApplyReport`], never a
//! bare `Err`. ADR 0011: the plan bytes reviewed are the plan bytes applied.

use super::{owner_index_options, MutationExecution};
use crate::apply::{apply_migration_plan, ApplyContext};
use norn_wire::MigrationPlan;
use norn_wire::{ApplyError, ApplyOutcome, ApplyReport};

/// Execute an `apply`: forecast (`confirm == false`) or apply (`confirm == true`)
/// the plan carried in `params`.
pub fn execute(
    cache: &crate::cache::Cache,
    config: Option<&crate::standards::VaultConfig>,
    params: &norn_wire::ApplyParams,
    _today: &str,
    sink: &mut crate::telemetry::EventSink,
) -> anyhow::Result<MutationExecution<ApplyReport>> {
    let index = cache.load_graph_index()?;
    let dry_run = !params.confirm;

    // The plan crossed opaque (the CLI parsed + schema-checked it before the wire).
    // A deserialize failure here is not the malformed-input case the CLI already
    // refuses — it is a wire-shape fault. Surface it as a coded, report-shaped
    // refusal (never a bare Err that would exit-to-heal a healthy owner).
    let plan: MigrationPlan = match serde_json::from_value(params.plan.clone()) {
        Ok(plan) => plan,
        Err(e) => {
            return Ok(refused(
                cache.vault_root().to_string(),
                dry_run,
                ApplyError {
                    code: "malformed-plan".into(),
                    message: format!("could not decode migration plan: {e}"),
                    path: None,
                },
            ));
        }
    };

    let ctx = ApplyContext {
        dry_run,
        parents: params.parents,
        verbose: false,
        refuse_as_report: true,
        owner_index_options: owner_index_options(config),
    };
    let apply_report = apply_migration_plan(&plan, &index, ctx, sink)?;
    // A forecast commits nothing (matches the sibling verbs): only a confirmed
    // apply hands the owner touched paths for the cache-increment commit.
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

/// Build a coded, report-shaped refusal (`outcome = refused`) with no touched
/// paths — the wire-decode fallback above.
fn refused(vault_root: String, dry_run: bool, error: ApplyError) -> MutationExecution<ApplyReport> {
    let report = ApplyReport::refused(vault_root, dry_run, "apply", error);
    debug_assert_eq!(report.outcome, ApplyOutcome::Refused);
    MutationExecution {
        report,
        touched_paths: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use norn_wire::{MigrationOp, MIGRATION_PLAN_SCHEMA_VERSION};
    use serde_json::json;
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

    /// A one-op `move_document` plan whose `vault_root` is the given real root.
    fn move_plan(root: &Utf8PathBuf, src: &str, dst: &str) -> MigrationPlan {
        MigrationPlan {
            schema_version: MIGRATION_PLAN_SCHEMA_VERSION,
            vault_root: root.to_string(),
            generator: None,
            generated_at: None,
            preconditions: Vec::new(),
            operations: vec![MigrationOp {
                kind: "move_document".into(),
                id: None,
                requires: Vec::new(),
                fields: json!({ "src": src, "dst": dst, "parents": false }),
                footnote: None,
            }],
            skipped: Vec::new(),
            plan_footnote: None,
        }
    }

    fn params(plan: &MigrationPlan, confirm: bool) -> norn_wire::ApplyParams {
        norn_wire::ApplyParams {
            plan: serde_json::to_value(plan).unwrap(),
            confirm,
            parents: false,
        }
    }

    #[test]
    fn apply_move_plan_writes_and_cascades() {
        let (_t, root) = synth_vault(&[
            ("a.md", "---\ntype: note\n---\n# A\n[[b]]\n"),
            ("b.md", "---\ntype: note\n---\n# B\n"),
        ]);
        let cache = built(&root);
        let plan = move_plan(&root, "b.md", "renamed.md");
        let exec = execute(&cache, None, &params(&plan, true), TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, ApplyOutcome::Applied);
        assert!(!exec.report.dry_run);
        assert!(root.join("renamed.md").as_std_path().exists());
        assert!(!root.join("b.md").as_std_path().exists());
        let a = std::fs::read_to_string(root.join("a.md").as_std_path()).unwrap();
        assert!(a.contains("[[renamed]]"), "backlink rewritten: {a}");
        assert!(!exec.touched_paths.is_empty());
    }

    #[test]
    fn dry_run_forecast_writes_nothing() {
        let (_t, root) = synth_vault(&[
            ("a.md", "---\ntype: note\n---\n# A\n[[b]]\n"),
            ("b.md", "---\ntype: note\n---\n# B\n"),
        ]);
        let cache = built(&root);
        let plan = move_plan(&root, "b.md", "renamed.md");
        let exec = execute(&cache, None, &params(&plan, false), TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, ApplyOutcome::Applied);
        assert!(exec.report.dry_run);
        assert!(root.join("b.md").as_std_path().exists());
        assert!(!root.join("renamed.md").as_std_path().exists());
        assert!(exec.touched_paths.is_empty());
    }

    #[test]
    fn vault_root_mismatch_refuses_as_report() {
        let (_t, root) = synth_vault(&[("a.md", "---\ntype: note\n---\n# A\n")]);
        let cache = built(&root);
        let mut plan = move_plan(&root, "a.md", "renamed.md");
        plan.vault_root = "/nonexistent/elsewhere".into();
        let exec = execute(&cache, None, &params(&plan, true), TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, ApplyOutcome::Refused);
        assert!(exec.touched_paths.is_empty());
        assert!(root.join("a.md").as_std_path().exists());
    }

    #[test]
    fn undecodable_plan_refuses_as_report() {
        let (_t, root) = synth_vault(&[("a.md", "---\ntype: note\n---\n# A\n")]);
        let cache = built(&root);
        // A value that is not a MigrationPlan shape (schema_version wrong type).
        let bad = norn_wire::ApplyParams {
            plan: json!({ "schema_version": "not-a-number" }),
            confirm: true,
            parents: false,
        };
        let exec = execute(&cache, None, &bad, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, ApplyOutcome::Refused);
        assert_eq!(
            exec.report.operations[0].error.as_ref().unwrap().code,
            "malformed-plan"
        );
        assert!(exec.touched_paths.is_empty());
    }

    /// A multi-op plan with two `{{seq}}` `create_document` ops allocates
    /// sequentially under one apply (the seq-alloc fold-in machinery): with an
    /// existing `task-000`, the two creates resolve to `task-1` and `task-2`
    /// (max+1, then folding in the just-allocated id). Proves NRN-101 sequential
    /// allocation across ops in a single plan.
    #[test]
    fn sequenced_seq_creates_allocate_in_order() {
        let (_t, root) = synth_vault(&[("tasks/task-000.md", "---\ntype: task\n---\n# T0\n")]);
        let cache = built(&root);
        let create = |body: &str| MigrationOp {
            kind: "create_document".into(),
            id: None,
            requires: Vec::new(),
            fields: json!({
                "path": "tasks/task-{{seq}}.md",
                "new_value": { "frontmatter": { "type": "task" }, "body": body },
            }),
            footnote: None,
        };
        let plan = MigrationPlan {
            schema_version: MIGRATION_PLAN_SCHEMA_VERSION,
            vault_root: root.to_string(),
            generator: None,
            generated_at: None,
            preconditions: Vec::new(),
            operations: vec![create("# One\n"), create("# Two\n")],
            skipped: Vec::new(),
            plan_footnote: None,
        };
        let exec = execute(&cache, None, &params(&plan, true), TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, ApplyOutcome::Applied);
        // Both creates landed on distinct, sequential ids (max existing + 1, +2).
        assert!(root.join("tasks/task-1.md").as_std_path().exists());
        assert!(root.join("tasks/task-2.md").as_std_path().exists());
        assert!(root.join("tasks/task-000.md").as_std_path().exists());
    }
}
