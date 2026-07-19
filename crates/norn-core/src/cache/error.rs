//! The cache engine's error type.
//!
//! Every corruption-class signal (a `rusqlite::Error`, an IO failure while
//! indexing) surfaces as a [`CacheError`]. The owner treats any such error as
//! fatal (ADR 0017 exit-to-heal): there is no integrity-check ladder, no
//! retry-once machinery, no rebuild-reason taxonomy — the db is disposable
//! derivation, so the owner terminates and a resummon rebuilds from scratch.

use camino::Utf8PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("io error at {path}: {source}")]
    Io {
        path: Utf8PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("failed to read file during indexing: {path}")]
    IndexRead {
        path: Utf8PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A staged increment observed its cache-side publication baseline change
    /// between reservation and terminal publish — an internal concurrent
    /// publication superseded it. Defends internal concurrent publication only
    /// (the data_version guard); not a filesystem re-proof.
    #[error("cache changed after the mutation's baseline was captured but before publication was reserved")]
    IncrementBaselineDrift,

    /// An affected source file changed on disk between the parse that produced
    /// the cache rows and the commit that would publish them — the re-read hash no
    /// longer matches the parsed content. Committing anyway would write stale
    /// content under a fresh `(mtime,size)` baseline that the stat-sweep probe
    /// would then read as Fresh forever. The refresh/increment aborts; the next
    /// probe re-fires.
    #[error("affected source changed while its cache rows were being committed: {path}")]
    IncrementSourceDrift { path: Utf8PathBuf },

    #[error("graph build error: {0}")]
    GraphBuild(#[from] crate::graph::IndexError),
}
