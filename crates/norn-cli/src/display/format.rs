//! The output-format vocabulary a command presents its Report in.
//!
//! Minimal by design this phase: `human` (the default terminal shape) and
//! `json` (the machine-readable envelope). The donor exposed a richer,
//! per-command `--format` vocabulary (`records` / `paths` / `jsonl` /
//! `markdown`); those land as the verbs that produce them port (NRN-329 wires
//! the per-command flags, the verb ports supply the renderers). Establishing
//! the pair now gives the display layer a stable presentation target.

use clap::ValueEnum;

/// How a command renders its Report to stdout.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// Human-legible terminal output (the default).
    #[default]
    Human,
    /// Machine-readable JSON.
    Json,
}
