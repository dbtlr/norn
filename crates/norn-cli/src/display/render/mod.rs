//! Per-verb renderers (NRN-409): one module per CLI verb, split out of the
//! former monolithic `display::emit`. [`super::emit`] still owns the single
//! `Output` → bytes dispatch; each module here holds the render function(s)
//! for its verb plus whatever helpers only that verb needs. [`shared`] holds
//! the handful of helpers more than one verb module reaches for.
//!
//! The renderers here are pinned to the donor CLI's output by the parity
//! suite: the `find` / `get` projections run through the shared
//! `output::projection` ladder; `count` / `describe` / `vault list` reproduce
//! their bespoke text unstyled (they never resolved a palette in the donor,
//! and their output is pinned by the parity cases).

pub(crate) mod apply;
pub(crate) mod count;
pub(crate) mod delete;
pub(crate) mod describe;
pub(crate) mod edit;
pub(crate) mod find;
pub(crate) mod get;
pub(crate) mod move_doc;
pub(crate) mod new;
pub(crate) mod repair;
pub(crate) mod rewrite_wikilink;
pub(crate) mod set;
mod shared;
pub(crate) mod validate;
pub(crate) mod vault;
