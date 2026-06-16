//! `edit` preflight: resolve target + build an empty plan via `set`'s preflight,
//! read the body, run the pure transform, then stamp the resulting body as a
//! single `replace_body` op through the shared `inject_body_change` seam.

use crate::edit::ops::EditOp;
use crate::edit::transform::{apply_edits, EditDescriptor};

pub struct EditPreflight {
    pub outcome: crate::set::synth::PreflightOutcome,
    pub descriptors: Vec<EditDescriptor>,
}

pub fn preflight_and_plan(
    cwd: &camino::Utf8Path,
    cache: &crate::cache::Cache,
    index: &crate::core::GraphIndex,
    cfg: &crate::standards::VaultConfig,
    target: &str,
    ops: &[EditOp],
) -> anyhow::Result<EditPreflight> {
    // Reuse set's preflight with zero frontmatter ops + no body to get a
    // resolved target and an empty RepairPlan. preflight does not read stdin
    // when body_from_stdin is false.
    let set_args = crate::cli::SetArgs {
        target: target.to_string(),
        fields: Vec::new(),
        field_json: Vec::new(),
        push: Vec::new(),
        pop: Vec::new(),
        remove: Vec::new(),
        body_from_stdin: false,
        force: false,
        yes: false,
        dry_run: false,
        format: crate::cli::SetFormat::Json,
    };
    let mut outcome = crate::set::synth::preflight_and_plan(cwd, cache, index, cfg, &set_args)?;

    // Read current body, run the pure transform.
    let full_path = cwd.join(&outcome.target);
    let content = std::fs::read_to_string(full_path.as_std_path())
        .map_err(|e| anyhow::anyhow!("failed to read {full_path}: {e}"))?;
    let current_body = crate::set::synth::body_after_frontmatter(&content);
    let transform = apply_edits(&current_body, ops)?;

    // Stamp the new body as a replace_body op via the shared seam.
    crate::set::synth::inject_body_change(cwd, &mut outcome, &transform.new_body)?;

    Ok(EditPreflight {
        outcome,
        descriptors: transform.descriptors,
    })
}
