//! The section-edit ENGINE: the op vocabulary and the pure body transform the
//! mutation executor's compose path applies for the section/body edit ops
//! (`str_replace` / `replace_section` / `append_to_section` / `delete_section` /
//! `insert_before_heading` / `insert_after_heading`, NRN-98).
//!
//! This is the engine half of `norn edit`; the `edit` VERB surface (route,
//! report, sugar/desugar, synth) ports later with that command. Kept here — not
//! deferred — because the executor structurally requires the transform primitive
//! to apply the section-edit ops a `MigrationPlan`/`RepairPlan` can carry, and
//! the ported orchestrator tests exercise it.

pub mod ops;
pub mod transform;
