//! `vault set <DOC>` command: schema-aware frontmatter mutation + wholesale
//! body replacement. Synthesizes a RepairPlan in-process and feeds it through
//! the existing apply_repair_plan orchestrator.

pub mod synth;

use anyhow::bail;
use anyhow::Result;
use camino::Utf8PathBuf;

use crate::cli::SetArgs;

pub fn run(_cwd: &Utf8PathBuf, _args: SetArgs) -> Result<i32> {
    bail!("vault set is not yet implemented")
}
