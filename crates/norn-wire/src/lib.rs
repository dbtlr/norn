#![forbid(unsafe_code)]
//! The Params/Report request/response vocabulary every side speaks — pure types.
//!
//! May never: Contain logic or IO, or depend on any other norn crate. Client-side crates depend on this instead of norn-core, which is what keeps "the client never opens a cache" compile-enforced.
//!
//! # Shipped vocabulary
//!
//! This crate is the typed successor to the donor tree's `route_wire.rs`: the
//! same wire encoding the CLI, MCP, and daemon sides exchange, expressed as
//! serde structs instead of imperative `serde_json::Map` inserts. A new
//! predicate is a struct field, not a forgotten map key.
//!
//! Phase 1 ships the SHARED vocabulary — verb-specific `Params`/`Report` types
//! land with their verbs:
//!
//! - [`FilterParams`] — the read-verb filter predicates.
//! - [`SortPaginateParams`] — the sort / limit / paging knobs.
//! - [`Presence`] — the tri-state absent / null / value distinction.
//!
//! # The wire's two load-bearing rules
//!
//! - **Defaults are omitted, never sent.** Empty lists, `None`, `false`, and the
//!   default `starts_at` all serialize to nothing; a fully-default value is `{}`.
//!   An absent key deserializes back to the default, so values round-trip.
//! - **Absent frontmatter is not empty frontmatter (NRN-222).** A source with no
//!   frontmatter block, a source with an empty `---`/`---` block, and a source
//!   with real frontmatter are three distinct wire states — [`Presence`] carries
//!   the distinction by construction so a re-rendered routed result cannot lose
//!   it.

mod control;
mod filter;
mod paging;
mod presence;
mod read;

pub use control::{ClientFrame, OwnerFrame, ServingState, WriterProgress, CONTROL_PROTOCOL};
pub use filter::FilterParams;
pub use paging::SortPaginateParams;
pub use presence::Presence;
pub use read::{CountParams, CountReport, FindDoc, FindParams, FindReport, GroupNode};

/// One-line boundary contract, referenced by every dependent so each
/// declared edge in the crate map is compiler-load-bearing.
pub const CONTRACT: &str = "norn-wire: the Params/Report vocabulary — pure types, no logic or IO";
