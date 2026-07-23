//! Warm-cache generations and the per-generation read-connection pool (ADR
//! 0013).
//!
//! # Generational contexts
//!
//! The held cache state is an immutable [`Generation`]: once opened, the
//! `(index identity, read pool, write connection)` binding never mutates. (The
//! DB *content* still changes — the writer-queue freshness refresh writes through
//! the generation's dedicated WRITE connection — but the *binding* is fixed.)
//! Every request binds the current `Arc<Generation>` at its boundary and holds it
//! to completion, so no request ever observes a swap. Reopen is single-flight
//! through the writer queue (see [`crate::cache::slot`]).
//!
//! # What the 0017 rewrite removed
//!
//! An earlier design threaded a `(dev, ino)` identity, a held-open sentinel `File`, and
//! an inode-reconciliation guard on every grown connection to defend against an
//! out-of-band `cache clear` swapping `cache.db` mid-request. The db is now pure
//! derivation owned by exactly one process for its lifetime (ADR 0017), so there
//! is no external swap to defend against: the ground-shift check, the sentinel,
//! the ABA lock-guard, and the invalidation floor are all deleted. Corruption is
//! exit-to-heal, not a floor bump.

use std::collections::BTreeSet;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Condvar, Mutex};

use camino::Utf8PathBuf;

use crate::cache::error::CacheError;
use crate::cache::Cache;

/// The index-relevant inputs that determine cache CONTENT. A generation records
/// the identity it was opened under; a config change whose new identity differs
/// (resolved index-set hash, `alias_field`, or `files.ignore`) is index-relevant.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct IndexIdentity {
    pub(crate) index_set_hash: String,
    pub(crate) alias_field: Option<String>,
    pub(crate) ignore: Vec<String>,
}

/// The inputs to open one additional read connection on lazy grow.
#[derive(Clone)]
pub(crate) struct GrowParams {
    pub(crate) db_path: Utf8PathBuf,
    pub(crate) vault_root: Utf8PathBuf,
    pub(crate) alias_field: Option<String>,
    pub(crate) files_ignore: Vec<String>,
    pub(crate) index_set: BTreeSet<String>,
    pub(crate) index_set_hash: String,
}

impl GrowParams {
    /// Open one additional read connection to the same db and stamp it
    /// `query_only`.
    fn open(&self) -> Result<Cache, CacheError> {
        let cache = Cache::open_secondary(
            &self.db_path,
            &self.vault_root,
            self.alias_field.as_deref(),
            &self.files_ignore,
            self.index_set.clone(),
            &self.index_set_hash,
        )?;
        cache.set_query_only()?;
        Ok(cache)
    }
}

/// Hard ceiling on read connections per generation, before clamping to available
/// parallelism.
const READ_POOL_MAX: usize = 8;
const READ_POOL_CAP_ENV: &str = "NORN_READ_POOL_CAP";

/// The per-generation read-connection cap: `min(READ_POOL_MAX, available
/// parallelism)`, floored at 1.
pub(crate) fn read_pool_cap() -> usize {
    let parallelism = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let default = parallelism.min(READ_POOL_MAX);
    crate::cache::engine::debug_env_usize(READ_POOL_CAP_ENV, default).max(1)
}

/// A per-generation POOL of `query_only` read connections to `cache.db` (ADR
/// 0013). Checkout pops an idle connection (LIFO — warmest first); with none idle
/// and under [`cap`](ReadPool::cap) it lazily grows one more; at cap it blocks on
/// [`available`](ReadPool::available) until a checkin. A grow failure degrades to
/// waiting for a checkin rather than failing the read (the pool is always
/// seeded).
pub(crate) struct ReadPool {
    inner: Mutex<ReadPoolInner>,
    available: Condvar,
    cap: usize,
    grow: GrowParams,
    #[cfg(test)]
    pub(crate) grow_opens: std::sync::atomic::AtomicU64,
}

struct ReadPoolInner {
    idle: Vec<Cache>,
    total: usize,
}

impl ReadPool {
    /// Seed the pool with the generation's already-opened primary read
    /// connection (stamping it `query_only` as it enters the pool).
    pub(crate) fn seed(
        primary: Cache,
        grow: GrowParams,
        cap: usize,
    ) -> Result<Arc<Self>, CacheError> {
        primary.set_query_only()?;
        Ok(Arc::new(ReadPool {
            inner: Mutex::new(ReadPoolInner {
                idle: vec![primary],
                total: 1,
            }),
            available: Condvar::new(),
            cap,
            grow,
            #[cfg(test)]
            grow_opens: std::sync::atomic::AtomicU64::new(0),
        }))
    }

    /// Check out a connection: reuse an idle one, else lazily grow while under
    /// cap, else block until a checkin frees one. Infallible.
    pub(crate) fn checkout(self: &Arc<Self>) -> PooledConn {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let mut try_grow = true;
        loop {
            if let Some(cache) = inner.idle.pop() {
                return PooledConn {
                    cache: Some(cache),
                    pool: Arc::clone(self),
                };
            }
            if try_grow && inner.total < self.cap {
                inner.total += 1;
                drop(inner);
                match self.grow.open() {
                    Ok(cache) => {
                        #[cfg(test)]
                        self.grow_opens
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        return PooledConn {
                            cache: Some(cache),
                            pool: Arc::clone(self),
                        };
                    }
                    Err(error) => {
                        eprintln!(
                            "norn: read-pool grow failed ({error}); \
                             waiting for an idle connection instead"
                        );
                        try_grow = false;
                        inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
                        inner.total -= 1;
                        self.available.notify_one();
                        continue;
                    }
                }
            }
            inner = self
                .available
                .wait(inner)
                .unwrap_or_else(|p| p.into_inner());
        }
    }

    fn checkin(&self, cache: Cache) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.idle.push(cache);
        drop(inner);
        self.available.notify_one();
    }
}

/// An owned, checked-out read-only connection from a generation's [`ReadPool`].
/// Derefs to the underlying [`Cache`]; on drop it returns the connection to its
/// pool. Holds an `Arc<ReadPool>` so it can check itself back in even if the
/// generation is dropping concurrently.
pub(crate) struct PooledConn {
    cache: Option<Cache>,
    pool: Arc<ReadPool>,
}

impl Deref for PooledConn {
    type Target = Cache;
    fn deref(&self) -> &Cache {
        self.cache
            .as_ref()
            .expect("pooled connection present until drop")
    }
}

impl DerefMut for PooledConn {
    fn deref_mut(&mut self) -> &mut Cache {
        self.cache
            .as_mut()
            .expect("pooled connection present until drop")
    }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if let Some(cache) = self.cache.take() {
            self.pool.checkin(cache);
        }
    }
}

/// An immutable warm-cache generation (ADR 0013): a read pool plus one write
/// connection to the same `cache.db`. Dropping the last `Arc` closes every
/// connection.
pub struct Generation {
    /// Monotonic generation number (1 for the first open).
    pub(crate) number: u64,
    /// The index identity this generation was opened under.
    pub(crate) index_identity: IndexIdentity,
    /// Request-facing READ connections. Every pooled connection is `query_only`.
    pub(crate) read_pool: Arc<ReadPool>,
    /// The WRITE connection, touched ONLY by the writer thread's ops (freshness
    /// refresh, increment commit). WAL mode makes the pooled read connections
    /// observe its committed rows across the connection boundary.
    pub(crate) write_cache: Mutex<Cache>,
    /// Coalescing state for freshness refreshes on this generation: the single
    /// in-flight-or-queued refresh ticket arriving requesters may join.
    pub(crate) refresh_pending: Mutex<Option<Arc<super::slot::RefreshTicket>>>,
}

impl Generation {
    /// Check out a read-only connection from this generation's pool for the life
    /// of one request.
    pub(crate) fn checkout_read(&self) -> PooledConn {
        self.read_pool.checkout()
    }
}
