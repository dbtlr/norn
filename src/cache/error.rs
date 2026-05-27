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

    // Superseded by `open::RebuildReason::IdentityDrift` (drift triggers a
    // silent rebuild rather than an error). Variant kept for now; safe to
    // delete in a cleanup pass.
    #[error("cache identity drift: cache was built against {cached}, current vault is {current}")]
    #[allow(dead_code)]
    IdentityDrift {
        cached: Utf8PathBuf,
        current: Utf8PathBuf,
    },

    #[error("cache schema version {found} is newer than this binary supports (expected {expected}); upgrade vault-cli")]
    SchemaNewer { found: u32, expected: u32 },

    #[error("vault root could not be canonicalized: {path}")]
    CannotCanonicalize {
        path: Utf8PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cache lock could not be acquired within timeout; another vault cache operation is in progress")]
    LockTimeout,

    #[error("failed to read file during indexing: {path}")]
    IndexRead {
        path: Utf8PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("graph build error: {0}")]
    GraphBuild(#[from] crate::graph::IndexError),
}
