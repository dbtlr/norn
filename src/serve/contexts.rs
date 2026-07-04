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
//! (each with its own NRN-55 `call_lock`) never contend.
//!
//! # Eviction
//!
//! Request-time `WarmContextError::RootGone` surfaces to the MCP client
//! per-request (it comes out of `query_cache` inside a tool handler and is
//! mapped by `to_mcp_error`); the daemon's connection loop never sees individual
//! tool errors, so there is no in-loop eviction hook. Instead the entry
//! self-heals lazily: on each `hello` we re-canonicalize the entry's stored root
//! (a cheap stat) and drop the entry if the root has vanished, so a later `hello`
//! rebuilds it cleanly. A vanished root with a live entry is otherwise harmless —
//! every request against it simply errors. There is no background reaper.

use std::collections::HashMap;
use std::sync::Arc;

use camino::Utf8Path;
use tokio::sync::{Mutex, OnceCell};

use crate::mcp::context::VaultContext;
use crate::mcp::server::McpServer;

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
        // Derive identity ourselves — never trust a client-supplied hash. A
        // canonicalize failure here means the root is gone/unreachable.
        let (canonical, hash) = crate::cache::vault_identity(Utf8Path::new(vault_root))
            .map_err(|_| anyhow::anyhow!("vault root does not exist: {vault_root}"))?;

        // Get-or-create the per-entry cell under a brief map lock. Also
        // self-heal: if an initialized entry's stored root has since vanished,
        // drop it so this hello rebuilds it.
        let cell = {
            let mut map = self.map.lock().await;
            if let Some(existing) = map.get(&hash) {
                if let Some(server) = existing.get() {
                    if std::fs::canonicalize(server.ctx.vault_root.as_std_path()).is_err() {
                        map.remove(&hash);
                    }
                }
            }
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

    /// Number of vaults currently tracked (initialized or mid-init). Test-only.
    #[cfg(test)]
    pub(crate) async fn len(&self) -> usize {
        self.map.lock().await.len()
    }
}

/// Build one warm [`McpServer`] for `canonical` with the FULL toolset (no
/// read-only mode — write safety remains the existing WriteLock flock + WAL, per
/// the decided design). Logs one stderr line on first-touch open.
fn open_server(canonical: &Utf8Path) -> anyhow::Result<McpServer> {
    let ctx = VaultContext::open_warm(canonical)?;
    eprintln!("norn serve: opened vault {canonical}");
    Ok(McpServer::new(Arc::new(ctx), /*read_only=*/ false))
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
}
