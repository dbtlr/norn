//! The `apply` execute seam: execute an already-reviewed `MigrationPlan`.
//!
//! Unlike the
//! other cascade verbs, `apply` does NOT synthesize a plan from a handful of
//! arguments — the CLI has already read the plan source (file or stdin), detected
//! its format, and parsed it into a `MigrationPlan` in the client-side preamble (a
//! malformed or unreadable source is a client diagnostic). The parsed plan crosses
//! TYPED in [`norn_wire::ApplyParams::plan`] (the `MigrationPlan` contract type now
//! lives in `norn-wire`); here the owner hands it straight to the shared
//! [`apply_migration_plan`] executor under `refuse_as_report` — so a clean
//! pre-write decline (an unsupported `schema_version`, an owner-set precondition
//! mismatch, a containment violation, a bad create path) returns a coded
//! `outcome = refused` [`ApplyReport`], never a bare `Err`. The `schema_version`
//! gate lives in that shared engine (audit-F3), not the CLI preamble, so the
//! routed/MCP surface is guarded too. ADR 0011: the plan bytes reviewed are the
//! plan bytes applied.

use super::{owner_index_options, MutationExecution};
use crate::apply::{apply_migration_plan, ApplyContext};
#[cfg(test)]
use norn_wire::ApplyOutcome;
use norn_wire::ApplyReport;
use norn_wire::MigrationPlan;

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

    // The plan crossed TYPED (NRN-405 part b): the CLI parsed + schema-checked it
    // before the wire, and the wire frame itself carries `MigrationPlan`, so a
    // malformed plan can no longer reach here as a decodable-but-wrong value —
    // a genuine wire-shape fault fails frame deserialization at the owner's read
    // loop, not here.
    let plan: &MigrationPlan = &params.plan;

    let ctx = ApplyContext {
        dry_run,
        parents: params.parents,
        verbose: false,
        refuse_as_report: true,
        owner_index_options: owner_index_options(config),
    };
    let apply_report = apply_migration_plan(plan, &index, ctx, sink)?;
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
            plan: plan.clone(),
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
        assert_eq!(exec.report.outcome, ApplyOutcome::Forecast);
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

    // NOTE (NRN-405 part b): the former `undecodable_plan_refuses_as_report` test
    // is removed. `ApplyParams::plan` is now a typed `MigrationPlan`, so a
    // "decodable-but-wrong-shape" plan value can no longer be constructed and
    // handed to `execute`; a genuine wire-shape fault fails frame deserialization
    // at the owner's read loop, before this seam. The client-side preamble still
    // refuses a malformed/schema-mismatched plan source before the wire.

    /// A multi-op plan with two `{{seq}}` `create_document` ops allocates
    /// sequentially under one apply (the seq-alloc fold-in machinery): with an
    /// existing `task-000`, the two creates resolve to `task-1` and `task-2`
    /// (max+1, then folding in the just-allocated id). Proves NRN-101 sequential
    /// allocation across ops in a single plan.
    /// The engine schema gate (audit-F3): a plan whose `schema_version` this build
    /// does not support refuses `unsupported-schema-version` (exit 2) before any
    /// work — zero operations examined, nothing written — on both an under- and
    /// over-version. The refusal now originates engine-side (the CLI preamble no
    /// longer checks), so this is where it is asserted; the refused report's
    /// `dry_run` tracks `confirm` (a forecast unless a real apply was requested)
    /// exactly as a successful apply's would.
    #[test]
    fn unsupported_schema_version_refuses_with_zero_ops_examined() {
        let (_t, root) = synth_vault(&[("a.md", "---\ntype: note\n---\n# A\n")]);
        let cache = built(&root);
        for bad_version in [0u32, 99u32] {
            for confirm in [false, true] {
                // A one-op plan whose op WOULD move a real doc if it ran — but the
                // schema gate refuses before expansion, so nothing is examined.
                let mut plan = move_plan(&root, "a.md", "renamed.md");
                plan.schema_version = bad_version;
                let exec =
                    execute(&cache, None, &params(&plan, confirm), TODAY, &mut sink()).unwrap();
                assert_eq!(
                    exec.report.outcome,
                    ApplyOutcome::Refused,
                    "schema_version {bad_version} must refuse"
                );
                assert_eq!(exec.report.exit_code(), 2);
                assert_eq!(exec.report.applied, 0);
                // The refused report is a forecast unless a real apply was confirmed.
                assert_eq!(exec.report.dry_run, !confirm);
                let err = exec
                    .report
                    .operations
                    .iter()
                    .find_map(|o| o.error.as_ref())
                    .expect("refused report carries a coded error");
                assert_eq!(err.code, "unsupported-schema-version");
                assert_eq!(
                    err.message,
                    format!(
                        "unsupported plan schema_version {bad_version}; this norn build supports v{}",
                        MIGRATION_PLAN_SCHEMA_VERSION
                    )
                );
                // Zero ops examined: nothing written, the doc is untouched.
                assert!(exec.touched_paths.is_empty());
                assert!(root.join("a.md").as_std_path().exists());
                assert!(!root.join("renamed.md").as_std_path().exists());
            }
        }
    }

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
