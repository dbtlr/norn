//! The lazy per-vault warm-context map.
//!
//! One `norn serve` daemon serves many vaults over a single socket, naming a
//! vault per connection via the `hello` frame. This module owns the map from a
//! vault's identity hash to its long-lived [`McpServer`] (which wraps a
//! verify-once warm [`VaultContext`], per ADR 0005: integrity is checked once
//! per vault, then maintained by the context's per-request self-heal).
//!
//! # Map shape and why
//!
//! `Mutex<HashMap<hash, Arc<OnceCell<McpServer>>>>` — the simplest shape that
//! satisfies the three hard requirements:
//!
//! - **(a) First-touch open is off the map lock and off the async workers.** The
//!   map `Mutex` is held only long enough to look up / insert the per-entry
//!   `Arc<OnceCell>`; it is released *before* the cell is initialized. The
//!   initializer runs the (potentially seconds-long) vault open inside
//!   [`tokio::task::spawn_blocking`], so opening a big vault never stalls pings,
//!   accepts, or other vaults.
//! - **(b) Concurrent first-touch for the same vault opens once.** A per-entry
//!   [`tokio::sync::OnceCell`] serializes initialization: the second concurrent
//!   `hello` for the same vault awaits (and shares) the first's result rather
//!   than opening a second context.
//! - **(non-poisoning) A failed open retries.** `get_or_try_init` leaves the
//!   cell empty on error, so the next `hello` for that vault attempts the open
//!   again.
//!
//! The identity hash is derived by the daemon itself from the `hello`'s
//! `vault_root` via [`crate::cache::vault_identity`] — a client-supplied hash is
//! never trusted. Distinct vaults hash to distinct keys, so their `McpServer`s
//! (each its own warm [`VaultContext`]) never contend. Warm requests do not take
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
use std::sync::Arc;

use camino::Utf8Path;
use tokio::sync::{Mutex, OnceCell};

use crate::mcp::context::VaultContext;
use crate::mcp::server::McpServer;
use crate::service::{ServingState, WriterProgress};

/// The lazy per-vault warm-context map. Cloneable via `Arc` at the call site.
pub(crate) struct Contexts {
    map: Mutex<HashMap<String, Arc<OnceCell<McpServer>>>>,
}

impl Contexts {
    pub(crate) fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
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
        let cell = {
            let mut map = self.map.lock().await;
            map.entry(hash)
                .or_insert_with(|| Arc::new(OnceCell::new()))
                .clone()
        };
        // Map lock released — the (possibly slow) open below runs unguarded.

        let server = cell
            .get_or_try_init(|| {
                let canonical = canonical.clone();
                async move {
                    // Requirement (a): the first-touch open (config parse now;
                    // integrity check + index build on the first query) must not
                    // run on an async worker.
                    tokio::task::spawn_blocking(move || open_server(&canonical))
                        .await
                        .map_err(|e| anyhow::anyhow!("vault open task failed: {e}"))?
                }
            })
            .await?;

        Ok(server.clone())
    }

    /// Observe one canonical vault's serving and writer state without opening
    /// it or touching the filesystem. `service status --vault` canonicalizes on
    /// the client side; hashing that path here reaches the same map entry as
    /// [`resolve`] while keeping the daemon's control path bounded to one brief
    /// map lookup plus lock-free atomics.
    pub(crate) async fn control_state(
        &self,
        canonical_root: &Utf8Path,
    ) -> (ServingState, WriterProgress) {
        let hash = crate::cache::canonical_vault_identity_hash(canonical_root);
        let cell = {
            let map = self.map.lock().await;
            map.get(&hash).cloned()
        };

        match cell {
            None => (ServingState::Cold, WriterProgress::default()),
            Some(cell) => match cell.get() {
                None => (ServingState::Opening, WriterProgress::default()),
                Some(server) => {
                    let progress = server.ctx.writer_progress();
                    (
                        ServingState::Ready,
                        WriterProgress {
                            busy: progress.busy,
                            sequence: progress.sequence,
                        },
                    )
                }
            },
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
        let snapshot: Vec<(String, Arc<OnceCell<McpServer>>, camino::Utf8PathBuf)> = {
            let map = self.map.lock().await;
            map.iter()
                .filter_map(|(hash, cell)| {
                    cell.get()
                        .map(|server| (hash.clone(), cell.clone(), server.ctx.vault_root.clone()))
                })
                .collect()
        };
        if snapshot.is_empty() {
            return;
        }

        // Stat each stored root off the lock.
        let dead: Vec<(String, Arc<OnceCell<McpServer>>)> =
            tokio::task::spawn_blocking(move || {
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

/// Remove entries in `dead` from `map`, but only when the stored `Arc` for
/// that hash is STILL the same one that was snapshotted as dead — a
/// concurrent re-insert under the same hash (a fresh open racing the sweep)
/// must never be clobbered. Extracted from [`Contexts::sweep_dead_roots`] so
/// the negative case (a re-inserted entry surviving a stale snapshot) is unit
/// testable without going through the async sweep/resolve machinery; no
/// behavior change.
fn remove_if_same_arc(
    map: &mut HashMap<String, Arc<OnceCell<McpServer>>>,
    dead: Vec<(String, Arc<OnceCell<McpServer>>)>,
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
fn open_server(canonical: &Utf8Path) -> anyhow::Result<McpServer> {
    let ctx = VaultContext::open_warm(canonical)?;
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
    /// absent entry = cold, empty OnceCell = opening, initialized server = ready.
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

        let cell = Arc::new(OnceCell::new());
        contexts.map.lock().await.insert(hash, Arc::clone(&cell));
        assert_eq!(
            contexts.control_state(&canonical).await,
            (ServingState::Opening, WriterProgress::default())
        );

        assert!(cell.set(open_server(&canonical).unwrap()).is_ok());
        let (serving, idle) = contexts.control_state(&canonical).await;
        assert_eq!(serving, ServingState::Ready);
        assert!(!idle.busy);

        let (running_tx, running_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let handle = cell
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
        let terminal = loop {
            let (_, progress) = contexts.control_state(&canonical).await;
            if !progress.busy {
                break progress;
            }
            tokio::task::yield_now().await;
        };
        assert!(!terminal.busy);
        assert!(terminal.sequence > busy.sequence);
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
        let mut map: HashMap<String, Arc<OnceCell<McpServer>>> = HashMap::new();
        let original: Arc<OnceCell<McpServer>> = Arc::new(OnceCell::new());
        map.insert("vault-hash".to_string(), original.clone());

        // Snapshot the entry as "dead" (as `sweep_dead_roots` would after a
        // stat failure on the stored root)...
        let dead = vec![("vault-hash".to_string(), original.clone())];

        // ...but before removal runs, a concurrent re-insert replaces the map
        // entry with a NEW `Arc` for the same hash (e.g. the root came back
        // and a fresh open raced the sweep).
        let replacement: Arc<OnceCell<McpServer>> = Arc::new(OnceCell::new());
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
