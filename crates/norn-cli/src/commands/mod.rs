//! One module per command. Each owns its clap `Args`, a `to_params` mapping
//! into the wire vocabulary, and a `run` entry that presents the outcome. The
//! dispatch match lives once, in [`crate::dispatch`].
//!
//! Two exemplars prove the pattern this phase — `find` and `get`. The rest of
//! the v0.48 surface (the `src/cli.rs` row of the porting burn-down) fills in
//! as one module each, NRN-329.

pub mod args;
pub mod find;
pub mod get;
