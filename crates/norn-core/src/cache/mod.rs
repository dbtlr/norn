//! The cache engine — an owner-opened SQLite projection of the vault graph
//! (ADRs 0003/0004/0013/0014/0017).
//!
//! The engine is a module owned by the summoned owner: it [`create`]s the db at
//! summon (schema + one-shot [`full_build`] warm-up) and the owner deletes it at
//! exit. No other process opens it. The db is pure derivation — corruption is
//! exit-to-heal (any error is fatal; a resummon rebuilds), so there is no
//! identity path matrix, no channel dirs, no self-heal ladder, no
//! reshred-on-open, and no per-operation write lock (a single writer thread by
//! construction).
//!
//! # Module map
//!
//! - [`engine`] — the [`Cache`] handle: lifecycle, `read_snapshot`,
//!   `observed_field_names`.
//! - [`schema`] / [`eav`] — the relational schema and the derived
//!   `document_fields` EAV writer (ADR 0004).
//! - [`writer`] — full build, the incremental freshness refresh, and the chunked
//!   mutation-increment pipeline (dormant in phase 2).
//! - [`reader`] / [`query_documents`] / [`find`] / [`query_show`] /
//!   [`query_links`] / [`query_diagnostics`] / [`live_examples`] — the read
//!   surface: `load_graph_index`, predicate SQL emission over the query layer's
//!   [`DocumentQuery`](crate::query::DocumentQuery), paged find, deep projection,
//!   status.
//! - [`freshness`] — the trust seam: the request-boundary
//!   [`FreshnessProbe`](freshness::FreshnessProbe).
//! - [`change_detection`] — the `.md` file-kind change detector shared by the
//!   refresh and the probe.
//! - [`writer_queue`] / [`generation`] / [`slot`] — the concurrency substrate:
//!   the two-class writer queue, generational read pool, and the owner-facing
//!   [`VaultCacheSlot`](slot::VaultCacheSlot).

pub(crate) mod canonical;
mod change_detection;
pub(crate) mod engine;
mod error;
mod find;
mod freshness;
mod generation;
mod invalidation;
mod live_examples;
mod query_diagnostics;
mod query_documents;
mod query_links;
mod query_show;
mod reader;
mod schema;
mod slot;
mod status;
mod writer;
pub mod writer_queue;

pub(crate) mod eav;

/// The relational schema version. Bumped when the DDL changes; stamped into the
/// `meta` table at [`Cache::create`].
pub(crate) const SCHEMA_VERSION: u32 = 5;

pub use change_detection::{ChangeDetectOptions, FileChange};
pub use engine::Cache;
pub use error::CacheError;
pub use find::{FindQuery, FindResult, SortClause, SortDirection};
pub use live_examples::{count_matching, field_statistics, FieldStats};
pub use query_show::{DocumentDeep, IncomingLink};
pub use slot::{ApplyIncrementOutcome, CacheOpenConfig, VaultCacheSlot};
pub use status::CacheStatus;
pub use writer::IndexReport;
pub use writer_queue::{
    ChunkOutcome, Handle, Outcome, WriterProgress, WriterProgressState, WriterQueue,
};

// Re-exported for the owner-side generation surface (`VaultCacheSlot`).
pub use generation::Generation;
