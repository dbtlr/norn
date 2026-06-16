//! `norn edit` — sub-document partial edits (NRN-19). See
//! artifacts/scratch/2026-06-16-norn-edit-design.md.
//
// NOTE: until the CLI/MCP wiring lands (synth/report + dispatch, downstream
// tasks), the pure transform core below is reached only from its own unit
// tests. Allow dead_code at the module boundary so the partial feature builds
// warning-clean; the downstream tasks that add real consumers remove this.
#![allow(dead_code)]

pub mod ops;
pub mod transform;
