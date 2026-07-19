//! The read verbs' execute seams (ADR 0016 Params/execute/Report).
//!
//! Each verb is a pure function of a warm [`Cache`](crate::cache::Cache) plus a
//! wire `Params`, producing a wire `Report`. The owner drives these inside
//! `serve_read`; the CLI renders the returned `Report`. No IO beyond the cache
//! read, no clock read (the current date is injected as `today`).
//!
//! Shipped this phase: [`find`] and [`count`]. `get` / `describe` follow.

pub mod count;
pub mod find;

use anyhow::Result;
use serde_json::Value;

use crate::cache::DocumentDeep;

/// The four deep-connection facets of a document, each pre-serialized to a JSON
/// value so the wire carries — and the CLI re-emits under `--format json` —
/// bytes byte-identical to the cache's own `Heading` / `Link` / `IncomingLink`
/// serialization. Shared by the `find` (opt-in via `--col`/`--all-cols`) and
/// `get` (always) read verbs so their deep-facet shapes cannot drift.
pub(crate) struct ConnectionValues {
    pub headings: Vec<Value>,
    pub outgoing_links: Vec<Value>,
    pub unresolved_links: Vec<Value>,
    pub incoming_links: Vec<Value>,
}

impl ConnectionValues {
    /// All-empty — the shape for a match whose connections were not loaded.
    pub(crate) fn empty() -> Self {
        Self {
            headings: Vec::new(),
            outgoing_links: Vec::new(),
            unresolved_links: Vec::new(),
            incoming_links: Vec::new(),
        }
    }
}

/// Serialize a [`DocumentDeep`]'s headings and three link sets into the wire's
/// pre-serialized JSON-value vectors.
pub(crate) fn connection_values(deep: &DocumentDeep) -> Result<ConnectionValues> {
    Ok(ConnectionValues {
        headings: to_values(&deep.headings)?,
        outgoing_links: to_values(&deep.outgoing_links)?,
        unresolved_links: to_values(&deep.unresolved_links)?,
        incoming_links: to_values(&deep.incoming_links)?,
    })
}

fn to_values<T: serde::Serialize>(items: &[T]) -> Result<Vec<Value>> {
    items
        .iter()
        .map(|item| serde_json::to_value(item).map_err(Into::into))
        .collect()
}
