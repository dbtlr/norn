//! The summoned owner runtime (ADR 0017 / 0013), Unix-only.
//!
//! One process, one vault. The owner binds its per-vault control socket, holds
//! the owner-lifetime flock, and creates one [`VaultCacheSlot`] whose SQLite db
//! lives in a per-owner temp dir DELETED on exit — the db is born-with-owner
//! disposable derivation (ADR 0017), so a client-vs-cache schema mismatch is
//! unrepresentable. Around the slot runs an async accept loop:
//!
//! - **Warm-up on summon.** The socket binds first (a summoning client can
//!   connect and observe `cold`/`opening`); the one-shot full build runs on a
//!   blocking thread, moving the serving state `cold → opening → ready`.
//! - **Control plane** (ADR 0013). A `ping` returns the vault's serving state
//!   plus `writer_progress { busy, sequence }` without touching the vault
//!   filesystem. There is no Direct fallback (0013's 2026-07-17 amendment): no
//!   pong means the client summons; a stalled busy writer is an owner-health
//!   event.
//! - **Routed read.** A `probe` runs the trivial document-count read through the
//!   slot's warm `serve_read` on a blocking thread — the stand-in exercised
//!   before the read verbs land next task.
//! - **Idle-TTL self-reap.** After `idle_ttl` with no request in flight, the
//!   owner shuts down: unbinds the socket and deletes the db. Bounds orphan
//!   lifetime to ~one TTL; the flock makes any orphan detectable.
//! - **Exit-to-heal.** Any [`CacheError`](norn_core::cache::CacheError) is
//!   fatal (the db is disposable) — the owner terminates and the next summon
//!   rebuilds. No integrity ladder, no retry.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use norn_core::cache::{CacheOpenConfig, VaultCacheSlot};
use norn_wire::{ClientFrame, OwnerFrame, ServingState, WriterProgress, CONTROL_PROTOCOL};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::lifecycle;

/// How often the idle reaper wakes to check the TTL. Small relative to the TTL
/// so reap latency is bounded without hot-spinning.
const REAP_CHECK_INTERVAL: Duration = Duration::from_millis(250);

/// Everything the summoner hands a freshly-spawned owner.
#[derive(Debug, Clone)]
pub struct OwnerConfig {
    /// The per-vault control socket to bind (already build-keyed by the client).
    pub socket_path: Utf8PathBuf,
    /// The canonical vault root this owner serves.
    pub vault_root: Utf8PathBuf,
    /// Idle time-to-live before self-reap.
    pub idle_ttl: Duration,
    /// The client's build fingerprint (short form), echoed in a pong for
    /// diagnostics / future tiers. The ephemeral client trusts the build-keyed
    /// socket, so this is informational.
    pub build: Option<String>,
}

/// The owner-lifetime lock path sits next to the socket: `<socket>.lock`.
fn lock_path_for(socket_path: &Utf8Path) -> Utf8PathBuf {
    Utf8PathBuf::from(format!("{socket_path}.lock"))
}

/// Shared owner state, reachable from the accept loop, warm-up, and reaper.
struct OwnerState {
    serving: Mutex<ServingState>,
    slot: Mutex<Option<Arc<VaultCacheSlot>>>,
    last_activity: Mutex<Instant>,
    in_flight: AtomicUsize,
    fatal: AtomicBool,
    build: Option<String>,
    shutdown: tokio::sync::Notify,
}

impl OwnerState {
    fn new(build: Option<String>) -> Self {
        Self {
            serving: Mutex::new(ServingState::Cold),
            slot: Mutex::new(None),
            last_activity: Mutex::new(Instant::now()),
            in_flight: AtomicUsize::new(0),
            fatal: AtomicBool::new(false),
            build,
            shutdown: tokio::sync::Notify::new(),
        }
    }

    fn touch(&self) {
        *self.last_activity.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
    }

    fn serving(&self) -> ServingState {
        *self.serving.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn set_serving(&self, state: ServingState) {
        *self.serving.lock().unwrap_or_else(|p| p.into_inner()) = state;
    }

    fn slot(&self) -> Option<Arc<VaultCacheSlot>> {
        self.slot.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn writer_progress(&self) -> WriterProgress {
        match self.slot() {
            Some(slot) => {
                let p = slot.writer_progress();
                WriterProgress {
                    busy: p.busy,
                    sequence: p.sequence,
                }
            }
            None => WriterProgress::default(),
        }
    }

    /// Trip exit-to-heal: mark fatal and signal shutdown. Any cache error routes
    /// here — the db is disposable, so the owner terminates and a resummon
    /// rebuilds (ADR 0017).
    fn go_fatal(&self) {
        self.fatal.store(true, Ordering::SeqCst);
        self.shutdown.notify_waiters();
    }

    fn request_shutdown(&self) {
        self.shutdown.notify_waiters();
    }
}

/// Run the owner to completion. Blocks until a shutdown signal, the idle TTL, or
/// a fatal cache error, then unbinds the socket and deletes the db. Returns an
/// exit code (0 clean/idle, 1 fatal exit-to-heal).
pub fn run(config: OwnerConfig) -> anyhow::Result<i32> {
    let runtime_dir = config
        .socket_path
        .parent()
        .map(Utf8Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("socket path has no parent dir: {}", config.socket_path))?;
    lifecycle::ensure_runtime_dir(&runtime_dir)?;

    let lock_path = lock_path_for(&config.socket_path);
    // Held for the whole process — dropping it releases single ownership, so it
    // must outlive the runtime below. Flock BEFORE bind so a losing racer never
    // clobbers the winner's socket.
    let _lock = lifecycle::acquire_owner_lock(&lock_path, env!("CARGO_PKG_VERSION"))?;

    // The db is born-with-owner: a per-owner temp dir under the (0700) runtime
    // dir, deleted when this handle drops at the end of `run` (clean/idle/fatal
    // all return through here). Co-locating it with the socket keeps every owner
    // runtime artifact under one owner-only dir.
    let db_dir = tempfile::Builder::new()
        .prefix("norn-owner-db-")
        .tempdir_in(runtime_dir.as_std_path())
        .map_err(|e| anyhow::anyhow!("failed to create owner db dir: {e}"))?;
    let db_path = Utf8PathBuf::from_path_buf(db_dir.path().join("cache.db"))
        .map_err(|p| anyhow::anyhow!("non-UTF8 db path: {}", p.display()))?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let socket_path = config.socket_path.clone();
    let exit = runtime.block_on(async move { serve(config, db_path).await });

    // Cleanup: unbind the socket + remove the lock file. The db dir drops with
    // `db_dir` here (deleting the disposable derivation). The flock releases as
    // `_lock` drops.
    let _ = std::fs::remove_file(socket_path.as_std_path());
    let _ = std::fs::remove_file(lock_path.as_std_path());
    drop(db_dir);
    drop(_lock);
    exit
}

async fn serve(config: OwnerConfig, db_path: Utf8PathBuf) -> anyhow::Result<i32> {
    use tokio::signal::unix::{signal, SignalKind};
    // Register signal handlers BEFORE binding so a registration failure leaves
    // no socket bound for a probe to mistake for a live owner.
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;

    let listener = lifecycle::bind_listener(&config.socket_path)?;

    let state = Arc::new(OwnerState::new(config.build.clone()));

    // Warm-up on summon: cold -> opening -> ready, on a blocking thread.
    {
        let state = Arc::clone(&state);
        let vault_root = config.vault_root.clone();
        tokio::spawn(async move {
            state.set_serving(ServingState::Opening);
            let build = tokio::task::spawn_blocking(move || {
                VaultCacheSlot::create(&db_path, &vault_root, CacheOpenConfig::default())
            })
            .await;
            match build {
                Ok(Ok(slot)) => {
                    *state.slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(slot));
                    state.set_serving(ServingState::Ready);
                }
                // A cache error during warm-up is exit-to-heal (ADR 0017): the
                // db never became valid, so terminate and let a resummon rebuild.
                Ok(Err(err)) => {
                    eprintln!("norn owner: warm-up failed (exit-to-heal): {err}");
                    state.go_fatal();
                }
                Err(join_err) => {
                    eprintln!("norn owner: warm-up task panicked: {join_err}");
                    state.go_fatal();
                }
            }
        });
    }

    // Idle-TTL reaper.
    {
        let state = Arc::clone(&state);
        let ttl = config.idle_ttl;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(REAP_CHECK_INTERVAL).await;
                if state.fatal.load(Ordering::SeqCst) {
                    return;
                }
                if state.in_flight.load(Ordering::SeqCst) == 0 {
                    let idle = state
                        .last_activity
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .elapsed();
                    if idle >= ttl {
                        state.request_shutdown();
                        return;
                    }
                }
            }
        });
    }

    // Accept loop.
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _addr)) => {
                    let state = Arc::clone(&state);
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, state).await {
                            eprintln!("norn owner: connection error: {e}");
                        }
                    });
                }
                Err(e) => {
                    eprintln!("norn owner: accept error: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            _ = state.shutdown.notified() => break,
            _ = sigint.recv() => break,
            _ = sigterm.recv() => break,
        }
    }

    if state.fatal.load(Ordering::SeqCst) {
        Ok(1)
    } else {
        Ok(0)
    }
}

/// Serve frames on one connection until EOF (the client may ping-until-ready
/// then probe on one connection). Each frame counts as activity, resetting the
/// idle TTL.
async fn handle_connection(stream: UnixStream, state: Arc<OwnerState>) -> anyhow::Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut reader = BufReader::new(rd);
    let mut line = String::new();

    loop {
        line.clear();
        // The peer is our own summoned client over a private per-vault socket
        // (0600, owner-only runtime dir), so the frame stream is trusted; a plain
        // line read is sufficient here.
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            break; // client closed the connection
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        state.in_flight.fetch_add(1, Ordering::SeqCst);
        state.touch();
        let response = match serde_json::from_str::<ClientFrame>(trimmed) {
            Ok(frame) => dispatch(&state, frame).await,
            Err(err) => OwnerFrame::Error {
                message: format!("malformed control frame: {err}"),
            },
        };
        state.touch();
        state.in_flight.fetch_sub(1, Ordering::SeqCst);

        let mut buf = serde_json::to_vec(&response)?;
        buf.push(b'\n');
        wr.write_all(&buf).await?;
        wr.flush().await?;

        if state.fatal.load(Ordering::SeqCst) {
            break;
        }
    }
    Ok(())
}

async fn dispatch(state: &Arc<OwnerState>, frame: ClientFrame) -> OwnerFrame {
    match frame {
        ClientFrame::Ping { protocol } => {
            if protocol != CONTROL_PROTOCOL {
                // Demoted sanity assert (ADR 0012 amendment): the socket is
                // build-keyed, so this should never fire — surface it if it does.
                return OwnerFrame::Error {
                    message: format!(
                        "control protocol mismatch: owner {CONTROL_PROTOCOL}, client {protocol}"
                    ),
                };
            }
            OwnerFrame::Pong {
                protocol: CONTROL_PROTOCOL,
                version: env!("CARGO_PKG_VERSION").to_string(),
                build: state.build.clone(),
                pid: std::process::id(),
                serving: state.serving(),
                writer_progress: state.writer_progress(),
            }
        }
        ClientFrame::Probe => {
            if state.serving() != ServingState::Ready {
                // The client pings-until-ready before probing; a probe arriving
                // early is not a cache fault — report, don't exit-to-heal.
                return OwnerFrame::Error {
                    message: "vault not ready".to_string(),
                };
            }
            let Some(slot) = state.slot() else {
                return OwnerFrame::Error {
                    message: "vault not ready".to_string(),
                };
            };
            let result = tokio::task::spawn_blocking(move || {
                slot.serve_read(|cache| Ok(cache.documents_matching(&Default::default())?.len()))
            })
            .await;
            match result {
                Ok(Ok(count)) => OwnerFrame::Probe {
                    document_count: count as u64,
                },
                // A cache error while serving is exit-to-heal (ADR 0017).
                Ok(Err(err)) => {
                    state.go_fatal();
                    OwnerFrame::Error {
                        message: format!("cache error (exit-to-heal): {err}"),
                    }
                }
                Err(join_err) => {
                    state.go_fatal();
                    OwnerFrame::Error {
                        message: format!("read task panicked (exit-to-heal): {join_err}"),
                    }
                }
            }
        }
    }
}
