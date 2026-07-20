//! One module per command. Each owns its clap `Args`, a `to_params` mapping
//! into the wire vocabulary, and a `run` entry that presents the outcome. The
//! dispatch match lives once, in [`crate::dispatch`].
//!
//! Two read exemplars prove the pattern for ported verbs — `find` and `get`,
//! which parse to Params then present the uniform not-yet-ported outcome. The
//! rest of the v0.48 read/mutation surface fills in as one module each,
//! NRN-329. `vault` is the intentionally-new registry surface (no oracle); it
//! is the first namespace that EXECUTES — its sub-verbs call `norn-config`
//! directly rather than deferring.

pub mod args;
pub mod count;
pub mod delete;
pub mod describe;
pub mod edit;
pub mod find;
pub mod get;
pub mod move_doc;
pub mod new;
pub mod rewrite_wikilink;
pub mod set;
pub mod validate;
pub mod vault;
