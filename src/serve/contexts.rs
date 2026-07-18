//! The lazy per-vault warm-context map.
//!
//! One `norn serve` daemon serves many vaults over a single socket, naming a
//! vault per connection via the `hello` frame. This module owns the map from a
//! vault's identity hash to its long-lived [`McpServer`] (which wraps a
//! verify-once warm [`VaultEnv`], per ADR 0005: integrity is checked once
//! per vault, then maintained by the context's per-request self-heal).
//!
//! # Map shape and why
//!
//! `Mutex<HashMap<hash, Arc<ContextEntry>>>` — the simplest shape that
//! satisfies the three hard requirements:
//!
//! - **(a) First-touch open is off the map lock and off the async workers.** The
//!   map `Mutex` is held only long enough to look up / insert the per-entry
//!   `Arc<ContextEntry>`; it is released *before* the cell is initialized. The
//!   initializer runs the (potentially seconds-long) vault open inside
//!   [`tokio::task::spawn_blocking`], so opening a big vault never stalls pings,
//!   accepts, or other vaults.
//! - **(b) Concurrent first-touch for the same vault opens once.** A per-entry
//!   shared attempt serializes initialization: the second concurrent `hello`
//!   awaits (and shares) the first's result rather than opening a second context;
//!   [`tokio::sync::OnceCell`] retains the successfully published server.
//! - **(non-poisoning) A failed open retries.** The failed shared attempt clears
//!   itself while the `OnceCell` remains unset, so the next `hello` for that
//!   vault starts a fresh attempt.
//!
//! The identity hash is derived by the daemon itself from the `hello`'s
//! `vault_root` via [`crate::cache::vault_identity`] — a client-supplied hash is
//! never trusted. Distinct vaults hash to distinct keys, so their `McpServer`s
//! (each its own warm [`VaultEnv`]) never contend. Warm requests do not take
//! the server's `call_lock` at all — it is the cold-only NRN-55 guard (NRN-253) —
//! so even concurrent requests to the SAME warm vault run in parallel.
//!
//! # Eviction
//!
//! Request-time `WarmContextError::RootGone` surfaces to the MCP client
//! per-request (it comes out of the per-request seam inside a tool handler and
//! is mapped by `to_mcp_error`); the daemon's connection loop never sees
//! individual tool errors, so there is no in-loop eviction hook. Instead the map
//! self-heals opportunistically: whenever an incoming `hello`'s OWN root fails to
//! canonicalize, [`Contexts::resolve`] sweeps the map for any *other* entries
//! whose stored root has vanished and evicts them, so a dead vault's warm context
//! (its SQLite fds + sentinel `File`) can't leak for the daemon's lifetime. The
//! sweep runs its blocking `stat`s OFF the map lock (see FIX-4), and removes an
//! entry only if it is still the same `Arc` it snapshotted, so a concurrent
//! re-insert is never clobbered. There is no background reaper.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use camino::Utf8Path;
use tokio::sync::{Mutex, OnceCell};

use crate::env::VaultEnv;
use crate::mcp::server::McpServer;
use crate::mcp::writer_queue::WriterProgressState;
use crate::service::{ServingState, WriterProgress};

struct ContextEntry {
    cell: OnceCell<McpServer>,
    opening: Arc<AtomicUsize>,
    progress: Arc<WriterProgressState>,
    attempt: StdMutex<Option<tokio::sync::watch::Receiver<Option<OpenResult>>>>,
    /// Last time this entry was resolved (a `hello`) or observed still in use by
    /// the idle-eviction sweep. Drives LRU ordering when the open-entry cap is
    /// enforced (NRN-337).
    last_touch: StdMutex<Instant>,
    /// Shared with the owning [`Contexts`]: the count of INITIALIZED entries
    /// (each pinning ~7 fds). Bumped when this entry's cell is first published and
    /// decremented in [`Drop`], so the count stays accurate across every removal
    /// path (cap eviction, dead-root sweep, in-flight-then-dropped) without a map
    /// walk — the pong reads it locklessly, preserving the O(1) control ping.
    open_entries: Arc<AtomicUsize>,
}

type OpenResult = Result<McpServer, String>;

impl ContextEntry {
    fn new(progress: Arc<WriterProgressState>, open_entries: Arc<AtomicUsize>) -> Self {
        Self {
            cell: OnceCell::new(),
            opening: Arc::new(AtomicUsize::new(0)),
            progress,
            attempt: StdMutex::new(None),
            last_touch: StdMutex::new(Instant::now()),
            open_entries,
        }
    }

    /// Refresh the LRU recency stamp to now.
    fn touch(&self) {
        *self.last_touch.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }

    /// The LRU recency stamp.
    fn touched_at(&self) -> Instant {
        *self.last_touch.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for ContextEntry {
    fn drop(&mut self) {
        // Pairs with the increment at cell publication: only an initialized entry
        // ever counted, so only an initialized entry decrements. `cell` is set at
        // most once and never unset, so this is exactly-once per counted entry —
        // and it fires whenever the LAST `Arc` drops (map removal AND any in-flight
        // request draining), i.e. exactly when the fds are actually released.
        if self.cell.get().is_some() {
            self.open_entries.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Hard ceiling on the number of INITIALIZED per-vault warm contexts the daemon
/// retains, enforced (LRU) at every entry-open (NRN-337). One shared daemon
/// serves every vault; each held context pins ~7 fds (entry `.lock`, the cache
/// db + WAL + SHM per pooled connection, the write connection, the sentinel), so
/// left unbounded a burst of distinct vaults (fixture / parity runs) exhausts the
/// process fd table (~256 on macOS) and wedges the daemon. Capping the retained
/// idle set bounds fd usage regardless of how many distinct vaults are served;
/// an active vault is never evicted (see [`Contexts::enforce_entry_cap`]).
///
/// `24 * ~7 ≈ 168` descriptors for a fully-idle retained set, leaving comfortable
/// headroom under 256 for a few concurrently-active vaults (whose read pools can
/// grow) plus the daemon's own baseline fds.
const MAX_OPEN_ENTRIES: usize = 24;

/// Debug/test-only override for [`MAX_OPEN_ENTRIES`] (read via
/// [`debug_env_usize`](crate::cache::debug_env_usize), so release builds ignore
/// it entirely). Lets an integration test pin a tiny cap to prove eviction
/// deterministically.
const MAX_OPEN_ENTRIES_ENV: &str = "NORN_SERVE_MAX_ENTRIES";

/// The retained-context cap: [`MAX_OPEN_ENTRIES`], or the debug-only env override,
/// floored at 1.
fn max_open_entries() -> usize {
    crate::cache::debug_env_usize(MAX_OPEN_ENTRIES_ENV, MAX_OPEN_ENTRIES).max(1)
}

#[derive(Clone)]
struct OpeningGuard {
    _inner: Arc<OpeningGuardInner>,
}

struct OpeningGuardInner {
    opening: Arc<AtomicUsize>,
}

impl OpeningGuard {
    fn new(opening: Arc<AtomicUsize>) -> Self {
        opening.fetch_add(1, Ordering::AcqRel);
        Self {
            _inner: Arc::new(OpeningGuardInner { opening }),
        }
    }
}

impl Drop for OpeningGuardInner {
    fn drop(&mut self) {
        self.opening.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The lazy per-vault warm-context map. Cloneable via `Arc` at the call site.
pub(crate) struct Contexts {
    map: Mutex<HashMap<String, Arc<ContextEntry>>>,
    /// Progress belongs to a canonical vault for the daemon lifetime, not to a
    /// disposable context entry. Retaining this tiny record prevents sequence
    /// regression when an evicted vault is later recreated.
    progress: StdMutex<HashMap<String, Arc<WriterProgressState>>>,
    /// Count of INITIALIZED entries (each pinning ~7 fds), maintained by
    /// [`ContextEntry`]'s publish/drop so it stays accurate across every removal
    /// path. Read locklessly for the `service status` open-entries field (NRN-337).
    open_entries: Arc<AtomicUsize>,
}

impl Contexts {
    pub(crate) fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            progress: StdMutex::new(HashMap::new()),
            open_entries: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Number of INITIALIZED per-vault contexts currently held open (each pins
    /// ~7 fds). Lockless — the daemon's pong reports it without taking the map
    /// lock, so the O(1) control ping stays O(1) (NRN-337).
    pub(crate) fn open_entries(&self) -> u64 {
        self.open_entries.load(Ordering::Acquire) as u64
    }

    /// Resolve the warm [`McpServer`] for `vault_root`, opening it lazily on
    /// first touch. See the module docs for the concurrency and eviction
    /// contract.
    ///
    /// Errors (returned so the caller can send an `Error` control frame):
    /// - the vault root does not exist / cannot be canonicalized;
    /// - the first-touch warm open failed (config parse error, etc.).
    pub(crate) async fn resolve(&self, vault_root: &str) -> anyhow::Result<McpServer> {
        // Derive identity ourselves — never trust a client-supplied hash. Run the
        // canonicalize on a blocking thread, NOT under the map lock and not on an
        // async worker: one hung/slow root (e.g. a stalled NFS mount) must not
        // stall hellos for every other vault (FIX-4).
        let owned_root = vault_root.to_string();
        let identity = tokio::task::spawn_blocking(move || {
            crate::cache::vault_identity(Utf8Path::new(&owned_root))
        })
        .await
        .map_err(|e| anyhow::anyhow!("vault identity task failed: {e}"))?;

        let (canonical, hash) = match identity {
            Ok(id) => id,
            Err(_) => {
                // The incoming root is gone/unreachable, so it has no live entry
                // to evict (its hash was never computed). Opportunistically sweep
                // OTHER dead-root entries so their warm contexts don't leak, then
                // return the hello error as before.
                self.sweep_dead_roots().await;
                return Err(anyhow::anyhow!("vault root does not exist: {vault_root}"));
            }
        };

        // Get-or-create the per-entry cell under a brief map lock. The old
        // per-hello stored-root re-check is deleted (FIX-4): a hash match here
        // implies the same canonical root that just canonicalized successfully
        // above, so the stored entry's root is live — and that re-check did
        // blocking `std::fs::canonicalize` WHILE HOLDING the async map lock.
        let progress = {
            let mut registry = self.progress.lock().unwrap_or_else(|e| e.into_inner());
            registry
                .entry(hash.clone())
                .or_insert_with(|| Arc::new(WriterProgressState::default()))
                .clone()
        };
        let entry = {
            let mut map = self.map.lock().await;
            let entry = map
                .entry(hash)
                .or_insert_with(|| {
                    Arc::new(ContextEntry::new(progress, Arc::clone(&self.open_entries)))
                })
                .clone();
            // A hello is activity — refresh LRU recency so a freshly-touched vault
            // is the LAST evicted.
            entry.touch();
            entry
        };
        // Map lock released — the (possibly slow) open below runs unguarded.

        // Bound retained fds: evict idle contexts (LRU) down to the cap before
        // this open proceeds (NRN-337). The entry we just took is held here (its
        // `Arc` count is > 1) and is not yet initialized, so it is never a
        // candidate for its own eviction.
        self.enforce_entry_cap().await;

        initialize_entry(entry, canonical, open_server).await
    }

    /// Evict idle warm contexts (LRU) until at most [`max_open_entries`] remain,
    /// bounding the daemon's open-fd footprint regardless of how many distinct
    /// vaults it serves (NRN-337).
    ///
    /// Safety invariants (all checked under the map lock, atomically with removal):
    /// - **Never evict an in-use context.** An entry is a candidate only when the
    ///   map is its SOLE holder (`Arc::strong_count(entry) == 1` → no concurrent
    ///   resolver), its cell is initialized, the stored `McpServer`'s `VaultEnv`
    ///   `Arc` count is 1 (→ no in-flight request holds a clone), it has no
    ///   in-flight open (`opening == 0`), and its writer queue is not busy
    ///   (`!progress.busy` → no undrained writer work). To serve OR resolve a
    ///   vault one must hold one of those `Arc`s, so this is a precise idle test.
    /// - **No torn context.** Removal only drops the map's `Arc`; a later request
    ///   for an evicted vault transparently reopens via [`resolve`] (re-acquiring
    ///   the entry lock and re-proving freshness — v0.48 re-proves per request, so
    ///   a reopened entry is safe by the existing path). An entry still held by a
    ///   draining request outlives its map removal and closes only when that last
    ///   `Arc` drops.
    /// - **Off-worker teardown.** Dropping an evicted `VaultEnv` joins its writer
    ///   thread (blocking), so the evicted `Arc`s are dropped on a blocking thread
    ///   rather than on an async worker (and off the map lock).
    async fn enforce_entry_cap(&self) {
        let cap = max_open_entries();
        let evicted: Vec<Arc<ContextEntry>> = {
            let mut map = self.map.lock().await;
            if map.len() <= cap {
                return;
            }
            // Idle candidates with their LRU stamp, oldest first. Uses `&Arc` from
            // the map so `strong_count == 1` genuinely means "map only".
            let mut idle: Vec<(String, Instant)> = map
                .iter()
                .filter(|(_, entry)| is_idle(entry))
                .map(|(hash, entry)| (hash.clone(), entry.touched_at()))
                .collect();
            idle.sort_by_key(|(_, touched)| *touched);

            let overflow = map.len() - cap;
            let mut evicted = Vec::new();
            for (hash, _) in idle.into_iter().take(overflow) {
                if let Some(entry) = map.remove(&hash) {
                    evicted.push(entry);
                }
            }
            evicted
        };
        if evicted.is_empty() {
            return;
        }
        let count = evicted.len();
        // VaultEnv `Drop` joins the per-vault writer thread — do it off the async
        // workers (and off the map lock).
        let _ = tokio::task::spawn_blocking(move || drop(evicted)).await;
        eprintln!("norn serve: evicted {count} idle vault context(s) to bound open descriptors");
    }

    /// Observe one canonical vault's serving and writer state without opening
    /// it or touching the filesystem. `service status --vault` canonicalizes on
    /// the client side; hashing that path here reaches the same map entry as
    /// [`resolve`] while keeping the daemon's control path bounded to one brief
    /// map lookup plus one coherent progress snapshot.
    pub(crate) async fn control_state(
        &self,
        canonical_root: &Utf8Path,
    ) -> (ServingState, WriterProgress) {
        let hash = crate::cache::canonical_vault_identity_hash(canonical_root);
        let entry = {
            let map = self.map.lock().await;
            map.get(&hash).cloned()
        };

        match entry {
            None => {
                let progress = self
                    .progress
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&hash)
                    .map_or_else(WriterProgress::default, |state| {
                        service_progress(state.snapshot())
                    });
                (ServingState::Cold, progress)
            }
            Some(entry) => {
                let serving = if entry.cell.get().is_some() {
                    ServingState::Ready
                } else if entry.opening.load(Ordering::Acquire) > 0 {
                    ServingState::Opening
                } else {
                    ServingState::Cold
                };
                (serving, service_progress(entry.progress.snapshot()))
            }
        }
    }

    /// Sweep the map for initialized entries whose stored vault root has vanished
    /// and evict them (FIX-4). The blocking `stat`s run OFF the map lock (on a
    /// blocking thread), so one hung root can't stall the lock for other hellos;
    /// an entry is removed only if it is still the SAME `Arc` we snapshotted, so a
    /// concurrent re-insert for the same hash is never clobbered.
    async fn sweep_dead_roots(&self) {
        // Snapshot (hash, cell, stored root) for INITIALIZED entries under a
        // brief lock. Uninitialized (mid-init) cells have no root yet and can't
        // be dead, so they are skipped.
        let snapshot: Vec<(String, Arc<ContextEntry>, camino::Utf8PathBuf)> = {
            let map = self.map.lock().await;
            map.iter()
                .filter_map(|(hash, cell)| {
                    cell.cell
                        .get()
                        .map(|server| (hash.clone(), cell.clone(), server.ctx.vault_root.clone()))
                })
                .collect()
        };
        if snapshot.is_empty() {
            return;
        }

        // Stat each stored root off the lock.
        let dead: Vec<(String, Arc<ContextEntry>)> = tokio::task::spawn_blocking(move || {
            snapshot
                .into_iter()
                .filter(|(_, _, root)| std::fs::canonicalize(root.as_std_path()).is_err())
                .map(|(hash, cell, _)| (hash, cell))
                .collect()
        })
        .await
        .unwrap_or_default();
        if dead.is_empty() {
            return;
        }

        // Re-acquire and remove only entries that are STILL the same `Arc`.
        let mut map = self.map.lock().await;
        remove_if_same_arc(&mut map, dead);
    }

    /// Number of vaults currently tracked (initialized or mid-init). Test-only.
    #[cfg(test)]
    pub(crate) async fn len(&self) -> usize {
        self.map.lock().await.len()
    }
}

/// Join or start the one detached open attempt for an entry. A resolver owns
/// only a result receiver, so canceling an initializer or waiter cannot cancel
/// work, retain an orphan retry, or create a duplicate open.
async fn initialize_entry<F>(
    entry: Arc<ContextEntry>,
    canonical: camino::Utf8PathBuf,
    opener: F,
) -> anyhow::Result<McpServer>
where
    F: FnOnce(&Utf8Path, Arc<WriterProgressState>) -> anyhow::Result<McpServer> + Send + 'static,
{
    if let Some(server) = entry.cell.get() {
        return Ok(server.clone());
    }

    let mut receiver = {
        let mut attempt = entry.attempt.lock().unwrap_or_else(|e| e.into_inner());
        // Close the cell/attempt TOCTOU: publication sets the cell before it
        // clears `attempt`, and both clearing and this recheck use this lock.
        if let Some(server) = entry.cell.get() {
            return Ok(server.clone());
        }
        if let Some(receiver) = attempt.as_ref() {
            receiver.clone()
        } else {
            let (sender, receiver) = tokio::sync::watch::channel(None);
            *attempt = Some(receiver.clone());

            let task_entry = Arc::clone(&entry);
            tokio::spawn(async move {
                let opening = OpeningGuard::new(Arc::clone(&task_entry.opening));
                let blocking_opening = opening.clone();
                let progress = Arc::clone(&task_entry.progress);
                let opened = tokio::task::spawn_blocking(move || {
                    let _opening = blocking_opening;
                    opener(&canonical, progress)
                })
                .await
                .map_err(|error| format!("vault open task failed: {error}"))
                .and_then(|result| result.map_err(|error| error.to_string()));

                let result = match opened {
                    Ok(server) => {
                        // Count this context as initialized as it is published —
                        // paired with the decrement in `ContextEntry::drop`
                        // (NRN-337). The `OnceCell` publishes at most once, so the
                        // increment fires exactly once per counted entry.
                        if task_entry.cell.set(server).is_ok() {
                            task_entry.open_entries.fetch_add(1, Ordering::AcqRel);
                        }
                        Ok(task_entry
                            .cell
                            .get()
                            .expect("successful open must publish the server")
                            .clone())
                    }
                    Err(error) => Err(error),
                };

                task_entry
                    .attempt
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                drop(opening);
                let _ = sender.send(Some(result));
            });

            receiver
        }
    };

    loop {
        if let Some(result) = receiver.borrow().clone() {
            return result.map_err(anyhow::Error::msg);
        }
        receiver
            .changed()
            .await
            .map_err(|_| anyhow::anyhow!("vault initializer stopped before publishing a result"))?;
    }
}

fn service_progress(progress: crate::mcp::writer_queue::WriterProgress) -> WriterProgress {
    WriterProgress {
        busy: progress.busy,
        sequence: progress.sequence,
    }
}

/// Remove entries in `dead` from `map`, but only when the stored `Arc` for
/// that hash is STILL the same one that was snapshotted as dead — a
/// concurrent re-insert under the same hash (a fresh open racing the sweep)
/// must never be clobbered. Extracted from [`Contexts::sweep_dead_roots`] so
/// the negative case (a re-inserted entry surviving a stale snapshot) is unit
/// testable without going through the async sweep/resolve machinery; no
/// behavior change.
/// Is `entry` idle and therefore safe to evict for the open-entry cap (NRN-337)?
///
/// True only when the map is the entry's sole holder AND its warm context is
/// initialized, fully released by every request, and quiescent — see
/// [`Contexts::enforce_entry_cap`] for why each clause is load-bearing. Must be
/// called while holding the map lock so the `strong_count` snapshots are coherent
/// with removal.
fn is_idle(entry: &Arc<ContextEntry>) -> bool {
    // The map is the only holder — no concurrent resolver has a clone.
    if Arc::strong_count(entry) != 1 {
        return false;
    }
    // No in-flight open racing this entry.
    if entry.opening.load(Ordering::Acquire) != 0 {
        return false;
    }
    // Only an initialized context pins fds and is worth evicting.
    let Some(server) = entry.cell.get() else {
        return false;
    };
    // No in-flight request holds a clone of the warm context.
    if Arc::strong_count(&server.ctx) != 1 {
        return false;
    }
    // No undrained writer-queue work.
    !entry.progress.snapshot().busy
}

fn remove_if_same_arc(
    map: &mut HashMap<String, Arc<ContextEntry>>,
    dead: Vec<(String, Arc<ContextEntry>)>,
) {
    for (hash, cell) in dead {
        if let Some(existing) = map.get(&hash) {
            if Arc::ptr_eq(existing, &cell) {
                map.remove(&hash);
            }
        }
    }
}

/// Build one warm [`McpServer`] for `canonical` with the FULL toolset — write
/// safety remains the existing WriteLock flock + WAL. Logs one stderr line on
/// first-touch open.
fn open_server(
    canonical: &Utf8Path,
    progress: Arc<WriterProgressState>,
) -> anyhow::Result<McpServer> {
    // Classify a poisoned-state open failure HERE, on the typed error, before the
    // OnceCell initializer stringifies it (`OpenResult = Result<_, String>`) —
    // after which the request path can no longer downcast it (NRN-337). A
    // hello-time open is always first-touch (no generation yet), so
    // `previously_served` is false: only fd exhaustion (the config/fingerprint
    // read hitting EMFILE) trips, never a first-touch cannot-open.
    let ctx = VaultEnv::open_warm_with_progress(canonical, progress)
        .inspect_err(|error| crate::serve::heal::maybe_trip(error, false))?;
    eprintln!("norn serve: opened vault {canonical}");
    // `new_daemon`: the daemon path emits the per-call served markers the
    // routing proofs count; a stdio `norn mcp` (plain `new`) never does.
    Ok(McpServer::new_daemon(Arc::new(ctx)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn seeded_vault() -> (TempDir, camino::Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-serve-contexts-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(
            root.join("alpha.md"),
            "---\ntype: note\nstatus: active\n---\nAlpha body\n",
        )
        .unwrap();
        (tmp, root)
    }

    /// Two resolves for the same vault share one entry (requirement b).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_same_vault_holds_one_entry() {
        let (_tmp, root) = seeded_vault();
        let contexts = Contexts::new();

        let a = contexts.resolve(root.as_str()).await;
        let b = contexts.resolve(root.as_str()).await;
        assert!(a.is_ok(), "first resolve: {:?}", a.err());
        assert!(b.is_ok(), "second resolve: {:?}", b.err());
        assert_eq!(contexts.len().await, 1, "same vault must map to one entry");
    }

    /// A nonexistent root is a clean error, not a panic, and creates no entry.
    #[tokio::test]
    async fn resolve_missing_root_errors() {
        let contexts = Contexts::new();
        let result = contexts.resolve("/no/such/vault/xyzzy").await;
        let err = match result {
            Ok(_) => panic!("missing root must error"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("vault root does not exist"),
            "unexpected error: {err}"
        );
        assert_eq!(contexts.len().await, 0);
    }

    /// The control plane reports the map lifecycle without opening the vault:
    /// absent entry = cold; an empty cell is opening only while its opening
    /// counter is nonzero, otherwise cold; an initialized server = ready.
    /// Once ready, writer progress comes from the live per-vault queue.
    #[tokio::test]
    async fn control_state_tracks_cold_opening_ready_and_writer_progress() {
        let (_tmp, root) = seeded_vault();
        let (canonical, hash) = crate::cache::vault_identity(&root).unwrap();
        let contexts = Contexts::new();

        assert_eq!(
            contexts.control_state(&canonical).await,
            (ServingState::Cold, WriterProgress::default())
        );

        let progress = Arc::new(WriterProgressState::default());
        let entry = Arc::new(ContextEntry::new(
            Arc::clone(&progress),
            Arc::new(AtomicUsize::new(0)),
        ));
        contexts.map.lock().await.insert(hash, Arc::clone(&entry));
        let opening = OpeningGuard::new(Arc::clone(&entry.opening));
        assert_eq!(
            contexts.control_state(&canonical).await,
            (ServingState::Opening, WriterProgress::default())
        );

        assert!(entry
            .cell
            .set(open_server(&canonical, progress).unwrap())
            .is_ok());
        drop(opening);
        let (serving, idle) = contexts.control_state(&canonical).await;
        assert_eq!(serving, ServingState::Ready);
        assert!(!idle.busy);

        let (running_tx, running_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let handle = entry
            .cell
            .get()
            .unwrap()
            .ctx
            .warm_writer_queue()
            .submit_liveness(move || {
                running_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            });
        running_rx.recv().unwrap();

        let (serving, busy) = contexts.control_state(&canonical).await;
        assert_eq!(serving, ServingState::Ready);
        assert!(
            busy.busy,
            "generation/liveness work must report writer busy"
        );
        assert!(busy.sequence > idle.sequence);

        release_tx.send(()).unwrap();
        assert_eq!(handle.wait(), crate::mcp::writer_queue::Outcome::Done(()));
        let (_, terminal) = contexts.control_state(&canonical).await;
        assert!(!terminal.busy);
        assert!(terminal.sequence > busy.sequence);
    }

    /// A failed `get_or_try_init` leaves its cell empty for a future retry, but
    /// with no initializer still running that state is cold, not opening.
    #[tokio::test]
    async fn failed_open_returns_to_cold_instead_of_sticking_opening() {
        let (_tmp, root) = seeded_vault();
        std::fs::create_dir_all(root.join(".norn")).unwrap();
        std::fs::write(root.join(".norn/config.yaml"), "validate: [not-a-map]").unwrap();
        let (canonical, _) = crate::cache::vault_identity(&root).unwrap();
        let contexts = Contexts::new();

        assert!(contexts.resolve(root.as_str()).await.is_err());
        let (serving, progress) = contexts.control_state(&canonical).await;
        assert_eq!(serving, ServingState::Cold);
        assert!(!progress.busy);

        std::fs::remove_file(root.join(".norn/config.yaml")).unwrap();
        contexts.resolve(root.as_str()).await.unwrap();
        assert_eq!(
            contexts.control_state(&canonical).await.0,
            ServingState::Ready,
            "the cold retained entry must remain retryable"
        );
    }

    /// Canceling a resolver detaches (rather than cancels) the cell initializer:
    /// status stays opening and a second resolver shares the same single open.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn canceled_resolver_keeps_single_flight_initializer_alive() {
        let (_tmp, root) = seeded_vault();
        let (canonical, hash) = crate::cache::vault_identity(&root).unwrap();
        let contexts = Contexts::new();
        let entry = Arc::new(ContextEntry::new(
            Arc::new(WriterProgressState::default()),
            Arc::new(AtomicUsize::new(0)),
        ));
        contexts.map.lock().await.insert(hash, Arc::clone(&entry));

        let open_count = Arc::new(AtomicUsize::new(0));
        let (running_tx, running_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first_count = Arc::clone(&open_count);
        let first = tokio::spawn(initialize_entry(
            Arc::clone(&entry),
            canonical.clone(),
            move |canonical, progress| {
                first_count.fetch_add(1, Ordering::SeqCst);
                running_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                open_server(canonical, progress)
            },
        ));

        running_rx.recv().unwrap();
        first.abort();
        match first.await {
            Err(error) => assert!(error.is_cancelled()),
            Ok(_) => panic!("aborted resolver unexpectedly completed"),
        }

        let second_count = Arc::clone(&open_count);
        let second = tokio::spawn(initialize_entry(
            Arc::clone(&entry),
            canonical.clone(),
            move |canonical, progress| {
                second_count.fetch_add(1, Ordering::SeqCst);
                open_server(canonical, progress)
            },
        ));
        tokio::task::yield_now().await;
        assert_eq!(
            contexts.control_state(&canonical).await.0,
            ServingState::Opening,
            "the detached initializer still owns the cell permit"
        );
        assert_eq!(
            open_count.load(Ordering::SeqCst),
            1,
            "the waiting resolver must not start a duplicate open"
        );

        release_tx.send(()).unwrap();
        second.await.unwrap().unwrap();
        assert_eq!(open_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            contexts.control_state(&canonical).await.0,
            ServingState::Ready,
            "the waiting resolver receives the detached initializer's server"
        );
    }

    /// A canceled waiter owns no retry. If the shared attempt fails, the entry
    /// stays cold until a later resolver explicitly starts the next attempt.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn canceled_waiter_does_not_retry_failed_open() {
        let (_tmp, root) = seeded_vault();
        let (canonical, _) = crate::cache::vault_identity(&root).unwrap();
        let entry = Arc::new(ContextEntry::new(
            Arc::new(WriterProgressState::default()),
            Arc::new(AtomicUsize::new(0)),
        ));
        let open_count = Arc::new(AtomicUsize::new(0));
        let (running_tx, running_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();

        let first_count = Arc::clone(&open_count);
        let first = tokio::spawn(initialize_entry(
            Arc::clone(&entry),
            canonical.clone(),
            move |_, _| {
                first_count.fetch_add(1, Ordering::SeqCst);
                running_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Err(anyhow::anyhow!("intentional first-open failure"))
            },
        ));
        running_rx.recv().unwrap();

        let waiter_count = Arc::clone(&open_count);
        let waiter = tokio::spawn(initialize_entry(
            Arc::clone(&entry),
            canonical.clone(),
            move |_, _| {
                waiter_count.fetch_add(1, Ordering::SeqCst);
                Err(anyhow::anyhow!("canceled waiter must never open"))
            },
        ));
        tokio::task::yield_now().await;
        waiter.abort();
        match waiter.await {
            Err(error) => assert!(error.is_cancelled()),
            Ok(_) => panic!("aborted waiter unexpectedly completed"),
        }

        release_tx.send(()).unwrap();
        match first.await.unwrap() {
            Err(error) => assert!(error.to_string().contains("intentional first-open failure")),
            Ok(_) => panic!("intentional first open unexpectedly succeeded"),
        }
        while entry.opening.load(Ordering::Acquire) != 0 {
            tokio::task::yield_now().await;
        }
        assert_eq!(open_count.load(Ordering::SeqCst), 1);
        assert!(entry.cell.get().is_none());

        let retry_count = Arc::clone(&open_count);
        initialize_entry(Arc::clone(&entry), canonical, move |canonical, progress| {
            retry_count.fetch_add(1, Ordering::SeqCst);
            open_server(canonical, progress)
        })
        .await
        .unwrap();
        assert_eq!(open_count.load(Ordering::SeqCst), 2);
        assert!(entry.cell.get().is_some());
    }

    /// The sequence belongs to the vault for the daemon lifetime, not to one
    /// disposable warm context. Evicting and reopening the same canonical root
    /// must never make a later observation regress.
    #[tokio::test]
    async fn writer_sequence_survives_context_eviction_and_recreation() {
        let (_tmp, root) = seeded_vault();
        let (canonical, hash) = crate::cache::vault_identity(&root).unwrap();
        let contexts = Contexts::new();
        contexts.resolve(root.as_str()).await.unwrap();

        let entry = contexts.map.lock().await.get(&hash).unwrap().clone();
        let handle = entry
            .cell
            .get()
            .unwrap()
            .ctx
            .warm_writer_queue()
            .submit_liveness(|| ());
        assert_eq!(handle.wait(), crate::mcp::writer_queue::Outcome::Done(()));
        let (_, before) = contexts.control_state(&canonical).await;
        assert!(before.sequence > 0);

        std::fs::remove_dir_all(&root).unwrap();
        assert!(contexts.resolve(root.as_str()).await.is_err());
        assert_eq!(contexts.len().await, 0, "dead context must be evicted");

        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("alpha.md"), "# Alpha\n").unwrap();
        contexts.resolve(root.as_str()).await.unwrap();
        let (_, after) = contexts.control_state(&canonical).await;
        assert!(
            after.sequence >= before.sequence,
            "same-daemon sequence regressed {} -> {}",
            before.sequence,
            after.sequence
        );
    }

    /// FIX-4: a later hello whose OWN root has vanished sweeps the map, evicting
    /// the now-dead entry rather than leaking its warm context for the daemon's
    /// lifetime.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_sweeps_dead_root_on_later_hello() {
        let (tmp, root) = seeded_vault();
        let contexts = Contexts::new();

        // First hello opens the vault → one live entry.
        contexts
            .resolve(root.as_str())
            .await
            .expect("first resolve");
        assert_eq!(contexts.len().await, 1, "one entry after first hello");

        // Delete the vault root out from under the daemon.
        drop(tmp);

        // A later hello for the (now-gone) root: canonicalize fails → error, and
        // the sweep evicts the dead entry.
        let err = match contexts.resolve(root.as_str()).await {
            Ok(_) => panic!("a vanished root must error"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("vault root does not exist"),
            "unexpected error: {err}"
        );
        assert_eq!(
            contexts.len().await,
            0,
            "the sweep must evict the dead-root entry"
        );
    }

    /// Negative case for the `Arc::ptr_eq` removal guard: a stale dead-root
    /// snapshot must NOT evict an entry that was re-inserted (a NEW `Arc`
    /// under the same hash) after the snapshot was taken. This is the race
    /// the ptr_eq check exists to close — without it a concurrent re-open
    /// racing the sweep would be silently clobbered.
    #[test]
    fn remove_if_same_arc_keeps_reinserted_entry() {
        let mut map: HashMap<String, Arc<ContextEntry>> = HashMap::new();
        let original = Arc::new(ContextEntry::new(
            Arc::new(WriterProgressState::default()),
            Arc::new(AtomicUsize::new(0)),
        ));
        map.insert("vault-hash".to_string(), original.clone());

        // Snapshot the entry as "dead" (as `sweep_dead_roots` would after a
        // stat failure on the stored root)...
        let dead = vec![("vault-hash".to_string(), original.clone())];

        // ...but before removal runs, a concurrent re-insert replaces the map
        // entry with a NEW `Arc` for the same hash (e.g. the root came back
        // and a fresh open raced the sweep).
        let replacement = Arc::new(ContextEntry::new(
            Arc::new(WriterProgressState::default()),
            Arc::new(AtomicUsize::new(0)),
        ));
        map.insert("vault-hash".to_string(), replacement.clone());

        remove_if_same_arc(&mut map, dead);

        let stored = map
            .get("vault-hash")
            .expect("the re-inserted entry must survive a stale dead-root snapshot");
        assert!(
            Arc::ptr_eq(stored, &replacement),
            "the surviving entry must be the NEW Arc, not evicted by the stale snapshot"
        );
    }
}
