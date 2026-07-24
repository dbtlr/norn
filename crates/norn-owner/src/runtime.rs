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

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use norn_core::cache::{Cache, VaultCacheSlot};
use norn_core::grammar::FieldRejection;
use norn_core::mutate::MutationExecution;
use norn_core::standards::VaultConfig;
use norn_core::telemetry::{Clock, EventSink, IdGen};
use norn_wire::{ClientFrame, OwnerFrame, ServingState, WriterProgress, CONTROL_PROTOCOL};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::lifecycle;

/// How often the idle reaper wakes to check the TTL. Small relative to the TTL
/// so reap latency is bounded without hot-spinning.
const REAP_CHECK_INTERVAL: Duration = Duration::from_millis(250);

/// How long shutdown waits for in-flight requests to drain before exiting, so a
/// request accepted at the reap boundary completes rather than being dropped
/// mid-flight (finding 3). Bounded so a client that never stops sending cannot
/// pin the owner open forever.
const DRAIN_BUDGET: Duration = Duration::from_secs(5);

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
    /// The resolved `[vaults.<name>].config` override path, if the summoning
    /// client passed one. `None` → the warm-up loads `<vault_root>/.norn/config.yaml`
    /// (the default) if present, else runs under the empty default config.
    pub config_path: Option<Utf8PathBuf>,
    /// The durable telemetry events dir (NRN-400), set ONLY for a registered
    /// vault. `Some` → confirmed mutations append to a daily JSONL file here and
    /// `audit` reads it back; `None` → the owner keeps in-memory (ephemeral)
    /// telemetry and `audit` returns an empty stream.
    pub events_dir: Option<Utf8PathBuf>,
}

/// The owner-lifetime lock path sits next to the socket: `<socket>.lock`.
fn lock_path_for(socket_path: &Utf8Path) -> Utf8PathBuf {
    Utf8PathBuf::from(format!("{socket_path}.lock"))
}

/// Shared owner state, reachable from the accept loop, warm-up, and reaper.
struct OwnerState {
    serving: Mutex<ServingState>,
    slot: Mutex<Option<Arc<VaultCacheSlot>>>,
    /// The vault's parsed config, loaded once at warm-up alongside the slot.
    /// `None` when the vault runs under no config file. `describe` reads it for
    /// its structure view (path rules, inbox, schema); the other read verbs need
    /// only the cache knobs already folded into the slot.
    vault_config: Mutex<Option<Arc<VaultConfig>>>,
    /// A warm-up USER error: a present-but-invalid `.norn/config.yaml` (bad
    /// YAML, unknown field, malformed rule; NRN-360), or a missing/non-directory
    /// vault root (a bad `-C`/NORN_ROOT, or a registered root that vanished after
    /// resolution; NRN-414). When set, every frame is answered with
    /// [`OwnerFrame::Rejected`] carrying this message — the established
    /// user-error path. Distinct from [`fatal`](Self::fatal)
    /// (exit-to-heal): a bad config is a user mistake, not a crashed owner, so
    /// the owner serves the error and exits CLEANLY (exit 0) instead of
    /// `go_fatal`. It also EAGER-REAPS: once a client has the rejection in hand,
    /// `handle_connection` latches shutdown (the socket key is config-blind, so
    /// a lingering bad-config owner would shadow every retry within the idle
    /// TTL). A resummon therefore spawns a FRESH owner that re-reads the config
    /// from disk — a fix is picked up immediately, never a crash loop and never
    /// a stale error.
    warmup_error: Mutex<Option<String>>,
    last_activity: Mutex<Instant>,
    in_flight: AtomicUsize,
    fatal: AtomicBool,
    build: Option<String>,
    /// The authoritative shutdown latch: once set it never clears, so a missed
    /// wakeup is recoverable — the accept loop polls it at the top of every
    /// iteration. `wake` only nudges the loop out of a blocking `accept`.
    shutdown_requested: AtomicBool,
    /// A permit-storing nudge (`notify_one`, NOT `notify_waiters`) so a
    /// notification landing between `select!` iterations is not lost — the next
    /// `notified()` returns immediately from the stored permit.
    wake: tokio::sync::Notify,
    /// The owner's in-process single-writer lock (ADR 0013/0017). Every mutation frame
    /// (`set`/`new`/…) holds it across its whole build-plan → apply → cache-commit
    /// critical section, so writes serialize and `{{seq}}` allocation observes
    /// prior creates. Reads never take it, so they stay concurrent.
    mutation_lock: tokio::sync::Mutex<()>,
    /// The durable telemetry events dir (NRN-400), `Some` only for a registered
    /// vault. A confirmed mutation appends to a daily JSONL file here; `audit`
    /// reads it back. `None` → in-memory telemetry, `audit` yields empty.
    events_dir: Option<Utf8PathBuf>,
    /// The `YYYY-MM-DD` day the retention sweep (`prune_events` +
    /// `enforce_size_cap`) last ran for, `None` before the first confirmed
    /// mutation. Amortizes the sweep to once per new day rather than once per
    /// mutation — see [`mark_swept_if_new_day`](Self::mark_swept_if_new_day).
    swept_day: Mutex<Option<String>>,
}

impl OwnerState {
    fn new(build: Option<String>, events_dir: Option<Utf8PathBuf>) -> Self {
        Self {
            serving: Mutex::new(ServingState::Cold),
            slot: Mutex::new(None),
            vault_config: Mutex::new(None),
            warmup_error: Mutex::new(None),
            last_activity: Mutex::new(Instant::now()),
            in_flight: AtomicUsize::new(0),
            fatal: AtomicBool::new(false),
            build,
            shutdown_requested: AtomicBool::new(false),
            wake: tokio::sync::Notify::new(),
            mutation_lock: tokio::sync::Mutex::new(()),
            events_dir,
            swept_day: Mutex::new(None),
        }
    }

    /// The durable telemetry events dir, `Some` only for a registered vault.
    fn events_dir(&self) -> Option<Utf8PathBuf> {
        self.events_dir.clone()
    }

    /// Whether the retention sweep should run for `today` (a `YYYY-MM-DD`
    /// string): `true` (and caches `today`) the first time this is called for
    /// a given day, `false` on every later call for the same day. Confirmed
    /// mutations are already serialized through `mutation_lock`, so this needs
    /// no synchronization beyond the plain `Mutex` — there is never a race
    /// between two callers deciding the same day is new.
    fn mark_swept_if_new_day(&self, today: &str) -> bool {
        let mut swept = self.swept_day.lock().unwrap_or_else(|p| p.into_inner());
        if swept.as_deref() == Some(today) {
            false
        } else {
            *swept = Some(today.to_string());
            true
        }
    }

    fn is_shutdown(&self) -> bool {
        self.shutdown_requested.load(Ordering::SeqCst)
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

    fn vault_config(&self) -> Option<Arc<VaultConfig>> {
        self.vault_config
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// The warm-up config error, if the vault's `.norn/config.yaml` failed to
    /// load. When `Some`, every frame is answered with a Rejected (NRN-360).
    fn warmup_error(&self) -> Option<String> {
        self.warmup_error
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Record a warm-up config error. Unlike [`go_fatal`](Self::go_fatal) this
    /// does NOT latch shutdown or mark fatal: the owner stays alive to serve the
    /// error and then idle-reaps cleanly (exit 0).
    fn set_warmup_error(&self, message: String) {
        *self.warmup_error.lock().unwrap_or_else(|p| p.into_inner()) = Some(message);
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

    /// Trip exit-to-heal: mark fatal and latch shutdown. Any cache error routes
    /// here — the db is disposable, so the owner terminates and a resummon
    /// rebuilds (ADR 0017).
    fn go_fatal(&self) {
        self.fatal.store(true, Ordering::SeqCst);
        self.request_shutdown();
    }

    /// Latch shutdown and nudge the accept loop. The latch (not the nudge) is
    /// authoritative, so this is lossless even if the nudge races a loop
    /// iteration.
    fn request_shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::SeqCst);
        self.wake.notify_one();
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
    let _lock = match lifecycle::acquire_owner_lock(&lock_path, env!("CARGO_PKG_VERSION"))? {
        lifecycle::AcquireOutcome::Acquired(file) => file,
        // Losing the summon race is NORMAL (finding 8): the incumbent owner
        // already serves this vault+build. Step aside cleanly (exit 0) — the
        // client connects to the incumbent. This info line lands in the per-owner
        // log, not the user's terminal.
        lifecycle::AcquireOutcome::Contended { incumbent_pid } => {
            match incumbent_pid {
                Some(pid) => eprintln!(
                    "norn owner: another owner already serves this vault (pid {pid}); stepping aside"
                ),
                None => eprintln!(
                    "norn owner: another owner already serves this vault; stepping aside"
                ),
            }
            return Ok(0);
        }
    };

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

    let state = Arc::new(OwnerState::new(
        config.build.clone(),
        config.events_dir.clone(),
    ));

    // Warm-up on summon: cold -> opening -> ready, on a blocking thread. The
    // JoinHandle is RETAINED (finding 2) so shutdown can await it before the db
    // dir is torn down — `spawn_blocking` cannot be aborted mid-`full_build`, so
    // awaiting is the only correct bound: it guarantees no build is still writing
    // into `db_dir` when `run` deletes it.
    let warmup_handle = {
        let state = Arc::clone(&state);
        let vault_root = config.vault_root.clone();
        let config_path = config.config_path.clone();
        tokio::spawn(async move {
            state.set_serving(ServingState::Opening);
            // Internal test/debug seam: an optional pre-build delay to simulate a
            // slow warm-up (only read when set).
            if let Some(ms) = std::env::var("NORN_OWNER_WARMUP_DELAY_MS")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
            {
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
            // Load the vault's config OFF the blocking thread's own read, then
            // build the cache under it — alias field, ignore globs, and the
            // resolved EAV index set all come from `.norn/config.yaml`. Without
            // this the cache would be built under the empty default config and
            // every alias/ignore/index decision would be wrong (ADR 0017).
            //
            // Two warm-up failure classes are kept distinct: a USER error (a
            // present-but-invalid config, NRN-360, OR a missing/non-directory
            // vault root, NRN-414) is surfaced to the client as a Rejected and
            // exits cleanly, while a cache-build fault is exit-to-heal (the db is
            // disposable derivation).
            enum WarmUp {
                // Boxed as one payload: the built slot + parsed config dwarf the
                // error string, and this enum is a one-shot warm-up return, so the
                // indirection is free (and keeps the variants size-balanced).
                Ready(Box<(VaultCacheSlot, Option<VaultConfig>)>),
                /// A warm-up USER error carried as its message: a present
                /// `.norn/config.yaml` that could not be read/parsed/validated
                /// (`invalid config <path>: <detail>`, NRN-360), or a
                /// missing/non-directory vault root (`vault root does not exist:
                /// <path>`, NRN-414). Both are user mistakes, not norn bugs.
                UserError(String),
            }
            let build = tokio::task::spawn_blocking(move || -> anyhow::Result<WarmUp> {
                // Vault-root user-error boundary (NRN-414): a missing or
                // non-directory vault root is a USER mistake (a bad -C/NORN_ROOT,
                // or a registered root that vanished after the client resolved
                // it), NOT a disposable-db fault. Classify it FIRST — before the
                // config load or the cache build — with the graph builder's own
                // message, so the graph-build failure never reaches the
                // exit-to-heal (fatal) arm below.
                if let Some(err) = norn_core::graph::vault_root_error(&vault_root) {
                    return Ok(WarmUp::UserError(err.to_string()));
                }
                // Config load is the next user-error boundary: any failure reading
                // or parsing the vault's own `.norn/config.yaml` is a user
                // mistake, NOT a disposable-db fault. Its message (`invalid config
                // <path>: <detail>`) is the user-facing config-error surface.
                let cache_config = match crate::config_load::load_cache_config(
                    &vault_root,
                    config_path.as_deref(),
                ) {
                    Ok(c) => c,
                    Err(e) => return Ok(WarmUp::UserError(e.to_string())),
                };
                // The full parsed config is retained for `describe`'s structure
                // view; the cache load above folds out only the four cache knobs.
                let vault_config = match crate::config_load::load_vault_config(
                    &vault_root,
                    config_path.as_deref(),
                ) {
                    Ok(c) => c,
                    Err(e) => return Ok(WarmUp::UserError(e.to_string())),
                };
                // Only a cache-build failure is exit-to-heal (the `?` below).
                let slot = VaultCacheSlot::create(&db_path, &vault_root, cache_config)?;
                Ok(WarmUp::Ready(Box::new((slot, vault_config))))
            })
            .await;
            match build {
                Ok(Ok(WarmUp::Ready(payload))) => {
                    let (slot, vault_config) = *payload;
                    *state.slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(slot));
                    *state.vault_config.lock().unwrap_or_else(|p| p.into_inner()) =
                        vault_config.map(Arc::new);
                    state.set_serving(ServingState::Ready);
                }
                // A warm-up USER error — a present-but-invalid config (NRN-360)
                // or a missing/non-directory vault root (NRN-414) — stores its
                // message so every frame is answered with Rejected, then exits
                // CLEANLY (exit 0). NOT go_fatal — a user mistake must not
                // crash-loop the summon; the connecting client surfaces the
                // message (`invalid config …` / `vault root does not exist: …`),
                // and the owner eager-reaps so a resummon re-reads a fixed config
                // or a restored root.
                Ok(Ok(WarmUp::UserError(message))) => {
                    eprintln!("norn owner: warm-up rejected (user error): {message}");
                    state.set_warmup_error(message);
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
        })
    };

    // Idle-TTL reaper. Loops until shutdown is observed (never one-shot): a
    // reap decision that races a late request is simply re-evaluated next tick,
    // so a single missed reap is recoverable, not a permanent orphan.
    {
        let state = Arc::clone(&state);
        let ttl = config.idle_ttl;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(REAP_CHECK_INTERVAL).await;
                if state.is_shutdown() {
                    // Re-nudge on the way out in case an earlier request raced
                    // the accept loop; the latch guarantees the loop still exits.
                    state.wake.notify_one();
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
                        // keep looping — next tick observes the latch and returns.
                    }
                }
            }
        });
    }

    // Accept loop. The shutdown latch is polled at the top of every iteration
    // (recovering any missed nudge); `wake.notified()` only unblocks a parked
    // `accept`.
    loop {
        if state.is_shutdown() {
            break;
        }
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
            _ = state.wake.notified() => break,
            _ = sigint.recv() => { state.request_shutdown(); break; }
            _ = sigterm.recv() => { state.request_shutdown(); break; }
        }
    }

    // Reaper TOCTOU (finding 3): stop accepting FIRST (drop the listener — a
    // connection racing the reap boundary is now either already accepted, and
    // drained below, or never accepted), then drain in-flight requests so none
    // is dropped mid-flight.
    drop(listener);
    let drain_deadline = Instant::now() + DRAIN_BUDGET;
    while state.in_flight.load(Ordering::SeqCst) > 0 && Instant::now() < drain_deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Await warm-up to completion before returning (finding 2): `run` deletes the
    // db dir the moment `serve` returns, and `full_build` may still be writing
    // into it. `spawn_blocking` cannot be cancelled mid-block, so awaiting is the
    // correct bound — it guarantees no orphaned temp files survive the disposable
    // db. For a shutdown that lands mid-warm-up this means shutdown latency is
    // bounded by the remaining warm-up time (~linear in vault size), which is
    // acceptable for the ephemeral tier.
    let _ = warmup_handle.await;

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
        // `dispatch` reports whether it took the warm-up-config-error path
        // (NRN-360) via its `warmup_reject` return. That single read of the
        // monotonic `warmup_error` is the SOLE authority for both the response
        // AND the eager-reap below. A prior design snapshotted `warmup_error`
        // here, before dispatch's own independent read; the two could disagree
        // when the error landed in the window between them, and dispatch would
        // serve the client a `Rejected` (the config error is in hand) while the
        // stale snapshot reported "not a warm-up reject" — so the owner skipped
        // the eager-reap AND `touch`ed the idle clock, then waited out the full
        // idle TTL (a ~30s reap-latency bug, NRN-391). Deriving the flag FROM
        // dispatch closes that window: one read decides both.
        let (response, warmup_reject) = match serde_json::from_str::<ClientFrame>(trimmed) {
            Ok(frame) => dispatch(&state, frame).await,
            Err(err) => (
                OwnerFrame::Error {
                    message: format!("malformed control frame: {err}"),
                },
                false,
            ),
        };
        // A warm-up config rejection can never serve a useful frame and is NOT
        // activity worth extending the owner's life, so it does not touch the
        // idle clock; every other frame resets it.
        if !warmup_reject {
            state.touch();
        }
        state.in_flight.fetch_sub(1, Ordering::SeqCst);

        let mut buf = serde_json::to_vec(&response)?;
        buf.push(b'\n');
        wr.write_all(&buf).await?;
        wr.flush().await?;

        if warmup_reject {
            // The client has now received the config error (the write+flush
            // above completed before this point), so eager-reap: latch a CLEAN
            // shutdown (never go_fatal) so a resummon spawns a FRESH owner that
            // re-reads `.norn/config.yaml` — a fix is picked up immediately
            // instead of after the full idle TTL against this stale-error owner.
            // The socket key is config-blind, so a lingering bad-config owner
            // would otherwise shadow every retry within the TTL window.
            state.request_shutdown();
            break;
        }
        if state.fatal.load(Ordering::SeqCst) {
            break;
        }
    }
    Ok(())
}

/// Dispatch one client frame to an owner response. The returned `bool` is the
/// authoritative `warmup_reject` flag: `true` iff this frame took the warm-up
/// config-error path, which `handle_connection` uses to eager-reap. Deriving it
/// from dispatch's OWN read of `warmup_error` (rather than a separate snapshot in
/// the caller) is load-bearing — see the race note at the call site (NRN-391).
///
/// CAUTION: the single-read invariant is not deterministically pinned by a test
/// (the None→Some transition window is timing-dependent). Reintroducing a
/// snapshot-then-dispatch split would NOT be caught by CI — it would resurface
/// as the rare 30s-TTL lingering-owner flake this fix closed.
async fn dispatch(state: &Arc<OwnerState>, frame: ClientFrame) -> (OwnerFrame, bool) {
    // A warm-up that failed on a USER error answers EVERY frame with that error
    // as a Rejected — the user-error path (an invalid config, NRN-360, or a
    // missing/non-directory vault root, NRN-414). The owner is healthy (not
    // fatal); it simply cannot serve a vault whose `.norn/config.yaml` it could
    // not parse, or whose root does not exist. The client renders the message;
    // `handle_connection` then eager-reaps so a resummon re-reads a fixed config
    // or a restored root.
    if let Some(message) = state.warmup_error() {
        return (
            OwnerFrame::Rejected {
                message,
                hints: Vec::new(),
            },
            true,
        );
    }
    (dispatch_frame(state, frame).await, false)
}

/// Dispatch a frame on a HEALTHY owner — [`dispatch`] handles the warm-up
/// config-error path (its single `warmup_error` read) before delegating here, so
/// every frame below runs its verb. Split out so that gate reads `warmup_error`
/// exactly once and this match's many `return not_ready()` early exits keep their
/// plain [`OwnerFrame`] type (NRN-391).
async fn dispatch_frame(state: &Arc<OwnerState>, frame: ClientFrame) -> OwnerFrame {
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
            // The routed-read stand-in (NRN-345). It shares the read verbs'
            // readiness gate — `ready_slot`/`not_ready`: an early probe is
            // reported, not a cache fault (the client pings-until-ready first) —
            // but keeps its own classification below, because it yields a bare
            // document count rather than a report-or-[`FieldRejection`] and so
            // does not fit [`classify_read`]'s success/user-rejection shape.
            let Some(slot) = ready_slot(state) else {
                return not_ready();
            };
            let result = tokio::task::spawn_blocking(move || {
                slot.serve_read(|cache| Ok(cache.documents_matching(&Default::default())?.len()))
            })
            .await;
            // Probe returns a bare count, never a report-or-`FieldRejection`, so it
            // keeps this local classifier rather than `classify_read` — the same
            // fault arms (cache error, task panic), minus the `Rejected` arm a
            // report-shaped read needs.
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
                        message: format!("probe task panicked (exit-to-heal): {join_err}"),
                    }
                }
            }
        }
        ClientFrame::Find { params } => {
            let today = today_local();
            let config = state.vault_config();
            dispatch_read(
                state,
                "find",
                move |cache| {
                    // NRN-367: gate the desugared dynamic fields against the
                    // vault's field universe BEFORE the query runs, so an unknown
                    // field rejects with a did-you-mean instead of silently
                    // matching nothing.
                    if let Err(rej) =
                        gate_query_fields(cache, config.as_deref(), &params.dynamic_keys)?
                    {
                        return Ok(Err(rej));
                    }
                    Ok(
                        norn_core::read::find::execute(cache, config.as_deref(), &params, &today)?
                            .map_err(FieldRejection::from),
                    )
                },
                |report| OwnerFrame::Find { report },
            )
            .await
        }
        ClientFrame::Count { params } => {
            let today = today_local();
            let config = state.vault_config();
            dispatch_read(
                state,
                "count",
                move |cache| {
                    // NRN-367: same field-universe gate as `find`.
                    if let Err(rej) =
                        gate_query_fields(cache, config.as_deref(), &params.dynamic_keys)?
                    {
                        return Ok(Err(rej));
                    }
                    Ok(
                        norn_core::read::count::execute(cache, config.as_deref(), &params, &today)?
                            .map_err(FieldRejection::from),
                    )
                },
                |report| OwnerFrame::Count { report },
            )
            .await
        }
        ClientFrame::Get { params } => {
            let today = today_local();
            dispatch_read(
                state,
                "get",
                move |cache| {
                    let outcome = norn_core::read::get::execute(cache, &params, &today)?;
                    Ok(match outcome {
                        Ok(mut report) => {
                            // `--format markdown` is the exact source file straight
                            // from disk (ADR 0014) — the OWNER reads it (it holds the
                            // vault root and is co-located with the vault; a future
                            // off-filesystem client could not). The multi-selection
                            // guard already ran in `execute` (NRN-460): a >1-selected
                            // request already carries its own error note by the time
                            // this runs, so `read_markdown_source` only performs the
                            // single-doc read. Both surfaces routed through this seam
                            // — CLI and MCP — see the same refusal signal (exit 1 /
                            // `isError: true` driven by that note); their payloads
                            // still differ (the CLI prints no document data on
                            // refusal, MCP returns the resolved records unconditionally).
                            if params.markdown {
                                read_markdown_source(cache, &mut report);
                            }
                            Ok(report)
                        }
                        Err(msg) => Err(FieldRejection::from(msg)),
                    })
                },
                |report| OwnerFrame::Get { report },
            )
            .await
        }
        ClientFrame::Describe { params } => {
            let config = state.vault_config();
            let today = today_local();
            dispatch_read(
                state,
                "describe",
                move |cache| {
                    // NRN-374: same field-universe gate as `find`/`count` — an
                    // unknown desugared dynamic field rejects with a did-you-mean
                    // instead of silently filtering the data-mode summary to
                    // nothing.
                    if let Err(rej) =
                        gate_query_fields(cache, config.as_deref(), &params.dynamic_keys)?
                    {
                        return Ok(Err(rej));
                    }
                    Ok(norn_core::read::describe::execute(
                        cache,
                        config.as_deref(),
                        &params,
                        &today,
                    )?
                    .map_err(FieldRejection::from))
                },
                |report| OwnerFrame::Describe { report },
            )
            .await
        }
        ClientFrame::Validate { params } => {
            let config = state.vault_config();
            let today = today_local();
            dispatch_read(
                state,
                "validate",
                move |cache| {
                    Ok(norn_core::read::validate::execute(
                        cache,
                        config.as_deref(),
                        &params,
                        &today,
                    )?
                    .map_err(FieldRejection::from))
                },
                |report| OwnerFrame::Validate { report },
            )
            .await
        }
        ClientFrame::Repair { params } => {
            let config = state.vault_config();
            let today = today_local();
            // Read-only (findings → plan, no write) — served through `serve_read`
            // like `validate`, NOT the single-writer mutation path.
            dispatch_read(
                state,
                "repair",
                move |cache| {
                    Ok(
                        norn_core::read::repair::execute(
                            cache,
                            config.as_deref(),
                            &params,
                            &today,
                        )?
                        .map_err(FieldRejection::from),
                    )
                },
                |report| OwnerFrame::Repair { report },
            )
            .await
        }
        ClientFrame::Audit { params } => {
            // Read-only over the durable event stream. The events dir is
            // owner-held (registration-gated); the cache is not consulted, so
            // the `dispatch_read` closure ignores it — but the readiness gate +
            // fault classification are still shared. A bad `--since`/`--until`
            // is a clean rejection (exit 1, the read-verb convention), never a
            // cache fault.
            let events_dir = state.events_dir();
            dispatch_read(
                state,
                "audit",
                move |_cache| {
                    use norn_core::telemetry::read;
                    let since = match params.since.as_deref().map(read::parse_since).transpose() {
                        Ok(v) => v,
                        Err(msg) => return Ok(Err(FieldRejection::from(msg))),
                    };
                    let until = match params.until.as_deref().map(read::parse_until).transpose() {
                        Ok(v) => v,
                        Err(msg) => return Ok(Err(FieldRejection::from(msg))),
                    };
                    // A reversed range (`--since` after `--until`) can never
                    // match an event — silently returning an empty report would
                    // read as "no events in range" rather than "the range is
                    // backwards". Reject it the same way as an unparseable date
                    // (exit 1, the read-verb convention) instead of failing open.
                    if let Err(msg) = read::validate_bounds(since, until) {
                        return Ok(Err(FieldRejection::from(msg)));
                    }
                    let filter = read::Filter {
                        trace: params.trace.clone(),
                        status: params.status.clone(),
                        target: params.target.clone(),
                        since,
                        until,
                    };
                    let events = match &events_dir {
                        Some(dir) => read::read_events(dir, &filter, params.limit),
                        None => Vec::new(),
                    };
                    Ok(Ok(norn_wire::AuditReport { events }))
                },
                |report| OwnerFrame::Audit { report },
            )
            .await
        }
        ClientFrame::Set { params } => {
            let config = state.vault_config();
            let today = today_local();
            dispatch_mutation(
                state,
                "set",
                params.confirm,
                move |cache, sink| {
                    norn_core::mutate::set::execute(cache, config.as_deref(), &params, &today, sink)
                },
                |report| OwnerFrame::Set { report },
            )
            .await
        }
        ClientFrame::New { params } => {
            let config = state.vault_config();
            let today = today_local();
            dispatch_mutation(
                state,
                "new",
                params.confirm,
                move |cache, sink| {
                    norn_core::mutate::new::execute(cache, config.as_deref(), &params, &today, sink)
                },
                |report| OwnerFrame::New { report },
            )
            .await
        }
        ClientFrame::Edit { params } => {
            let config = state.vault_config();
            let today = today_local();
            dispatch_mutation(
                state,
                "edit",
                params.confirm,
                move |cache, sink| {
                    norn_core::mutate::edit::execute(
                        cache,
                        config.as_deref(),
                        &params,
                        &today,
                        sink,
                    )
                },
                |report| OwnerFrame::Edit { report },
            )
            .await
        }
        ClientFrame::Move { params } => {
            let config = state.vault_config();
            let today = today_local();
            dispatch_mutation(
                state,
                "move",
                params.confirm,
                move |cache, sink| {
                    norn_core::mutate::move_doc::execute(
                        cache,
                        config.as_deref(),
                        &params,
                        &today,
                        sink,
                    )
                },
                |report| OwnerFrame::Move { report },
            )
            .await
        }
        ClientFrame::Delete { params } => {
            let config = state.vault_config();
            let today = today_local();
            dispatch_mutation(
                state,
                "delete",
                params.confirm,
                move |cache, sink| {
                    norn_core::mutate::delete::execute(
                        cache,
                        config.as_deref(),
                        &params,
                        &today,
                        sink,
                    )
                },
                |report| OwnerFrame::Delete { report },
            )
            .await
        }
        ClientFrame::RewriteWikilink { params } => {
            let config = state.vault_config();
            let today = today_local();
            dispatch_mutation(
                state,
                "rewrite-wikilink",
                params.confirm,
                move |cache, sink| {
                    norn_core::mutate::rewrite_wikilink::execute(
                        cache,
                        config.as_deref(),
                        &params,
                        &today,
                        sink,
                    )
                },
                |report| OwnerFrame::RewriteWikilink { report },
            )
            .await
        }
        ClientFrame::Apply { params } => {
            let config = state.vault_config();
            let today = today_local();
            dispatch_mutation(
                state,
                "apply",
                params.confirm,
                move |cache, sink| {
                    norn_core::mutate::apply::execute(
                        cache,
                        config.as_deref(),
                        &params,
                        &today,
                        sink,
                    )
                },
                |report| OwnerFrame::Apply { report },
            )
            .await
        }
    }
}

/// Owns the routed-READ scaffold shared by every read verb (NRN-411): the
/// ready-slot gate, the blocking `serve_read`, and outcome classification. An arm
/// supplies only its per-verb `read` body and a report→frame mapping; the
/// readiness gate ([`ready_slot`]/[`not_ready`]) and the success-vs-fault split
/// ([`classify_read`]) live here once.
///
/// `read` runs on a blocking thread and returns
/// `anyhow::Result<Result<R, FieldRejection>>`: the outer `anyhow` is a
/// cache/read fault (exit-to-heal), the inner `Result` a success or a user
/// rejection (bad predicate, unknown field). Reads never take the writer lock, so
/// they stay concurrent.
async fn dispatch_read<R: Send + 'static>(
    state: &Arc<OwnerState>,
    verb: &str,
    read: impl FnOnce(&Cache) -> anyhow::Result<Result<R, FieldRejection>> + Send + 'static,
    to_frame: impl FnOnce(R) -> OwnerFrame,
) -> OwnerFrame {
    let Some(slot) = ready_slot(state) else {
        return not_ready();
    };
    let result = tokio::task::spawn_blocking(move || slot.serve_read(read)).await;
    classify_read(state, result, to_frame, verb)
}

/// Owns the routed-MUTATION scaffold shared by every mutation verb AND the
/// owner's single-writer lock (ADR 0013/0017, NRN-411): the ready-slot gate, lock
/// acquisition, the blocking critical section, and outcome classification.
///
/// `mutation_lock` has exactly one acquisition site: this function. Every
/// mutation arm that calls through this seam carries no lock line of its own, so
/// the per-arm "forgot to take the lock" mistake is eliminated for those arms —
/// there is no lock line left to omit. The guard is held across the whole
/// blocking section so a second mutation waits and `{{seq}}` allocation observes
/// prior creates.
///
/// This does not make a bypass impossible: a hand-rolled arm could still reach
/// `slot.serve_read` directly and write without ever calling this seam (the
/// `Probe` arm above proves the shape compiles). That bypass is closed by
/// `mutation_dispatch_guard`'s test, not by construction — it scans every
/// mutation-class `ClientFrame` arm and fails if one does not route through
/// `dispatch_mutation`.
async fn dispatch_mutation<R: Send + 'static>(
    state: &Arc<OwnerState>,
    verb: &str,
    confirm: bool,
    execute: impl FnOnce(&Cache, &mut EventSink) -> anyhow::Result<MutationExecution<R>>
        + Send
        + 'static,
    to_frame: impl FnOnce(R) -> OwnerFrame,
) -> OwnerFrame {
    /// Drive a mutation verb's execute seam against the warm slot and, when a
    /// CONFIRMED write landed, commit its cache increment so the next read
    /// observes it (fire-and-degrade — a stale baseline heals on the next read).
    /// The mutation runs inside `serve_read` (the write goes to the filesystem
    /// under the index root, independent of the read connection); the caller
    /// ([`dispatch_mutation`]) already holds the owner's `mutation_lock` across
    /// this call, so no writer races it — and, being nested here, this primitive
    /// is unreachable without that lock.
    ///
    /// `confirm` gates the baseline capture: a forecast (`confirm == false`) never
    /// writes and never commits. Returns the verb's report; a clean pre-write
    /// decline is carried IN the report (`outcome = refused`), so only a genuine
    /// cache/read fault is the `Err` (exit-to-heal).
    fn run_mutation<R>(
        slot: &Arc<VaultCacheSlot>,
        confirm: bool,
        events_dir: Option<&Utf8Path>,
        mark_swept_if_new_day: impl FnOnce(&str) -> bool,
        execute: impl FnOnce(&Cache, &mut EventSink) -> anyhow::Result<MutationExecution<R>>,
    ) -> anyhow::Result<R> {
        // Durable telemetry (NRN-400) is registration-gated AND confirmed-only:
        // a registered vault's CONFIRMED mutation appends to the daily JSONL
        // store under `events_dir`, minting a real trace id the report carries;
        // a forecast (writes nothing) and an ephemeral vault (no events dir) both
        // keep the in-memory discard sink. `IdGen::new()` gives a process-unique
        // trace id so distinct invocations never collide in the store.
        let mut sink = match (confirm, events_dir) {
            (true, Some(dir)) => {
                let start_ts = Clock::System.now_rfc3339();
                // Best-effort retention sweep, amortized to once per new day (not
                // once per mutation, NRN-400 review): the sweep only visits the
                // events dir the first time a confirmed mutation lands on a day
                // the owner has not yet swept.
                //
                // Deliberate bound: both `prune_events` (age) and
                // `enforce_size_cap` (total bytes) constrain PRIOR days' files
                // only — today's file is never removed by either, so it is
                // unbounded by design. The store never drops a record from the
                // day still being written; `enforce_size_cap` fires a distinct
                // warning when today's file alone is already over the cap
                // (retention cannot correct that by sweeping older days).
                if mark_swept_if_new_day(&start_ts[..10]) {
                    norn_core::telemetry::store::prune_events(
                        dir,
                        norn_core::telemetry::store::DEFAULT_RETENTION,
                        &start_ts[..10],
                    );
                    norn_core::telemetry::store::enforce_size_cap(
                        dir,
                        norn_core::telemetry::store::EVENTS_SIZE_CAP_BYTES,
                        &start_ts[..10],
                    );
                }
                EventSink::open(dir, start_ts, IdGen::new(), Clock::System)
                    .unwrap_or_else(|_| EventSink::discard(IdGen::new(), Clock::System))
            }
            _ => EventSink::discard(IdGen::new(), Clock::System),
        };
        // Capture the pre-write baseline for the increment commit BEFORE the write.
        let baseline = if confirm {
            Some(slot.serve_read(|cache| cache.load_graph_index())?)
        } else {
            None
        };
        let exec = slot.serve_read(|cache| execute(cache, &mut sink))?;
        if let Some(baseline) = baseline {
            if !exec.touched_paths.is_empty() {
                // Fire-and-degrade: a failed increment leaves the next read's
                // detect to heal the cache; the write itself already landed.
                let _ =
                    slot.commit_apply_increments_fire_and_degrade(&exec.touched_paths, baseline);
            }
        }
        Ok(exec.report)
    }

    let Some(slot) = ready_slot(state) else {
        return not_ready();
    };
    // Serialize every write through the owner's single-writer lock, acquired in
    // this shared seam rather than per-arm so it can never be omitted.
    let _writer = state.mutation_lock.lock().await;
    let events_dir = state.events_dir();
    let sweep_state = Arc::clone(state);
    let result = tokio::task::spawn_blocking(move || {
        run_mutation(
            &slot,
            confirm,
            events_dir.as_deref(),
            move |today| sweep_state.mark_swept_if_new_day(today),
            execute,
        )
    })
    .await;
    classify_mutation(state, result, to_frame, verb)
}

/// Classify a routed mutation's outcome. Unlike a read, a mutation never yields a
/// `FieldRejection` — every clean decline is a report with `outcome = refused` —
/// so the only non-success is a cache/read fault (exit-to-heal) or a task panic.
fn classify_mutation<R>(
    state: &Arc<OwnerState>,
    result: Result<anyhow::Result<R>, tokio::task::JoinError>,
    to_frame: impl FnOnce(R) -> OwnerFrame,
    verb: &str,
) -> OwnerFrame {
    match result {
        Ok(Ok(report)) => to_frame(report),
        Ok(Err(err)) => {
            state.go_fatal();
            OwnerFrame::Error {
                message: format!("cache error (exit-to-heal): {err}"),
            }
        }
        Err(join_err) => {
            state.go_fatal();
            OwnerFrame::Error {
                message: format!("{verb} task panicked (exit-to-heal): {join_err}"),
            }
        }
    }
}

/// Fill `markdown_content` with the exact source bytes for a single-doc
/// `--format markdown` request. The multi-selection guard (NRN-460) lives in
/// `norn_core::read::get::execute` (co-located with the sort/paging skip it
/// depends on), so by the time this runs `report.records.len() > 1` has already
/// pushed its own `format-markdown-multi-selection` error note and left
/// `markdown_content` untouched; this function only performs the single-doc
/// read. A read failure becomes an `error`-severity note (a read verb's exit-1 /
/// `isError: true` signal) rather than a cache fault — the db is intact; the
/// file just could not be read. Zero resolved is left to the per-target
/// `target-not-found` notes `execute` already pushed.
fn read_markdown_source(cache: &norn_core::cache::Cache, report: &mut norn_wire::GetReport) {
    if report.records.len() != 1 {
        return;
    }
    let path = report.records[0].path.clone();
    let full = cache.vault_root().join(&path);
    match std::fs::read_to_string(full.as_std_path()) {
        Ok(raw) => report.markdown_content = Some(raw),
        Err(_) => report.notes.push(norn_wire::Note::error(
            "source-read-failed",
            format!("could not read source file for '{path}'"),
        )),
    }
}

/// The warm slot when serving is Ready, else `None` (the client pings-until-ready
/// before a read; an early read is reported, not a fault — see [`not_ready`]).
fn ready_slot(state: &Arc<OwnerState>) -> Option<Arc<VaultCacheSlot>> {
    if state.serving() != ServingState::Ready {
        return None;
    }
    state.slot()
}

/// The "vault not ready" report a read gets before warm-up finishes.
fn not_ready() -> OwnerFrame {
    OwnerFrame::Error {
        message: "vault not ready".to_string(),
    }
}

/// Gate a query's dynamically-desugared field keys against the vault's field
/// universe (NRN-367). The universe is the schema-declared fields (the vault
/// config's validate rules + the configured alias field) unioned with the
/// frontmatter keys actually observed in the cache; a dynamic key outside it is
/// a mistyped field or flag and is rejected with a did-you-mean via the one
/// shared [`closest`](norn_core::grammar::closest) heuristic. Canonical
/// `--eq`/`--in` keys never reach here (the CLI omits them from `dynamic_keys`),
/// so an explicit predicate on an as-yet-unseen field is never gated — only the
/// forgiving `--field value` desugar is.
///
/// The `Ok(Ok(()))` / `Ok(Err(rejection))` / `Err(_)` split mirrors the read
/// verbs': a rejection is a user error (owner stays alive), while a cache error
/// reading the observed keys propagates as exit-to-heal.
fn gate_query_fields(
    cache: &Cache,
    config: Option<&VaultConfig>,
    dynamic_keys: &[String],
) -> anyhow::Result<Result<(), FieldRejection>> {
    if dynamic_keys.is_empty() {
        return Ok(Ok(()));
    }
    let mut universe: BTreeSet<String> = match config {
        Some(cfg) => norn_core::grammar::schema_field_names(&cfg.validate, cfg),
        None => BTreeSet::new(),
    };
    universe.extend(cache.observed_field_names()?);
    let known_flags = norn_core::grammar::frozen_known_flags().query_known_flags();
    Ok(norn_core::grammar::gate_dynamic_fields(
        dynamic_keys,
        &universe,
        &known_flags,
    ))
}

/// Classify a routed read's outcome into an owner frame. The read verbs return
/// `anyhow::Result<Result<Report, FieldRejection>>` (a plain user-error string
/// maps into a hint-less [`FieldRejection`]; the field-universe gate supplies
/// the did-you-mean hints):
///
/// - `Ok(Ok(report))` — success → the verb's report frame.
/// - `Ok(Err(rejection))` — a user error (bad predicate, unresolvable target,
///   unknown dynamic field) → [`OwnerFrame::Rejected`] carrying the headline and
///   any soft-landing hints. The owner stays alive.
/// - `Err(_)` / join panic — a cache/read fault → exit-to-heal (ADR 0017).
fn classify_read<R>(
    state: &Arc<OwnerState>,
    result: Result<anyhow::Result<Result<R, FieldRejection>>, tokio::task::JoinError>,
    to_frame: impl FnOnce(R) -> OwnerFrame,
    verb: &str,
) -> OwnerFrame {
    match result {
        Ok(Ok(Ok(report))) => to_frame(report),
        Ok(Ok(Err(rejection))) => OwnerFrame::Rejected {
            message: rejection.message,
            hints: rejection.hints,
        },
        Ok(Err(err)) => {
            state.go_fatal();
            OwnerFrame::Error {
                message: format!("cache error (exit-to-heal): {err}"),
            }
        }
        Err(join_err) => {
            state.go_fatal();
            OwnerFrame::Error {
                message: format!("{verb} task panicked (exit-to-heal): {join_err}"),
            }
        }
    }
}

/// Today's date as `%Y-%m-%d` in the caller's LOCAL timezone, injected into the
/// read verbs so `--on today` resolves against the user's wall clock (NRN-359).
///
/// Local, not UTC: `--on today` must resolve against the user's wall clock. A
/// UTC resolution silently shifts the day boundary by up to a day near midnight,
/// which is wrong for a user asking about "today".
///
/// jiff over chrono: jiff is a single, focused datetime crate with first-class
/// local-zone support — `TimeZone::system()` reads the system zone (honoring
/// `TZ`) — and no feature-flag juggling or the historical `localtime_r`
/// soundness caveats chrono's `clock` feature carries. The whole need here is
/// "the local civil date, as a string," which jiff expresses directly.
fn today_local() -> String {
    today_in(jiff::Timestamp::now(), &jiff::tz::TimeZone::system())
}

/// The civil date (`%Y-%m-%d`) of `now` as seen in `tz`. Factored out of
/// [`today_local`] so the timezone-awareness is deterministically testable with
/// an explicit zone and a fixed instant (jiff caches the process system zone, so
/// mutating the `TZ` env mid-process is not a reliable test lever).
fn today_in(now: jiff::Timestamp, tz: &jiff::tz::TimeZone) -> String {
    now.to_zoned(tz.clone()).strftime("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    /// NRN-359: `today` resolves against the LOCAL wall clock, not UTC. At an
    /// instant just after UTC midnight, a far-east zone has already rolled to
    /// the new day while a far-west zone is still on the previous one — a
    /// UTC-only resolver would return the same date for both. Uses an explicit
    /// zone + fixed instant (the deterministic form of the `TZ=UTC+X boundary`
    /// case) since jiff caches the process system zone.
    #[test]
    fn today_resolves_in_the_local_zone_not_utc() {
        let just_after_utc_midnight: jiff::Timestamp = "2026-07-20T00:30:00Z".parse().unwrap();
        let east = today_in(
            just_after_utc_midnight,
            &jiff::tz::TimeZone::get("Etc/GMT-14").unwrap(), // UTC+14
        );
        let west = today_in(
            just_after_utc_midnight,
            &jiff::tz::TimeZone::get("Etc/GMT+12").unwrap(), // UTC-12
        );
        assert_eq!(east, "2026-07-20", "UTC+14 has already rolled to the 20th");
        assert_eq!(west, "2026-07-19", "UTC-12 is still on the 19th");
        assert_ne!(east, west, "the zone must change the resolved date");
    }

    /// NRN-360: once a warm-up config error is recorded, EVERY frame — ping,
    /// probe, and the read verbs — is answered with a `Rejected` carrying the
    /// config message (the user-error path), never a `go_fatal` Error frame.
    #[test]
    fn warmup_config_error_rejects_every_frame() {
        let state = Arc::new(OwnerState::new(None, None));
        state.set_warmup_error(
            "invalid config /vault/.norn/config.yaml: unknown field `not`".to_string(),
        );

        // A ping (normally a Pong) is rejected with the config message, and
        // dispatch flags the eager-reap (`warmup_reject == true`) from its own
        // read — the single authority `handle_connection` relies on (NRN-391).
        let (frame, warmup_reject) = block_on(dispatch(
            &state,
            ClientFrame::Ping {
                protocol: CONTROL_PROTOCOL,
            },
        ));
        assert!(
            warmup_reject,
            "a warm-up config error must flag the eager-reap"
        );
        match frame {
            OwnerFrame::Rejected { message, .. } => {
                assert!(
                    message.starts_with("invalid config "),
                    "expected the `invalid config` message, got {message:?}"
                );
            }
            other => panic!("expected Rejected, got {other:?}"),
        }

        // A probe is rejected too — the owner cannot serve reads under a config
        // it could not parse — and likewise flags the eager-reap.
        assert!(matches!(
            block_on(dispatch(&state, ClientFrame::Probe)),
            (OwnerFrame::Rejected { .. }, true)
        ));

        // Recording a config error must NOT trip the fatal (exit-to-heal) latch:
        // a bad config is a user mistake, so the owner exits cleanly, not fatal.
        // Nor does recording it itself latch shutdown — the eager-reap latch is
        // pulled by `handle_connection` only AFTER a client has been served the
        // rejection, so the first client reliably receives it.
        assert!(
            !state.fatal.load(Ordering::SeqCst),
            "a config error must not mark the owner fatal"
        );
        assert!(
            !state.is_shutdown(),
            "recording a config error must not itself latch shutdown"
        );
    }

    /// NRN-400 (review): the retention sweep (`prune_events` +
    /// `enforce_size_cap`) is amortized to once per new day, not once per
    /// confirmed mutation. `mark_swept_if_new_day` is the gate `run_mutation`
    /// consults — this pins its own trigger logic directly, independent of the
    /// filesystem-touching sweep functions themselves.
    #[test]
    fn mark_swept_if_new_day_fires_once_per_day() {
        let state = OwnerState::new(None, None);

        assert!(
            state.mark_swept_if_new_day("2026-07-23"),
            "the first call for a day must trigger the sweep"
        );
        assert!(
            !state.mark_swept_if_new_day("2026-07-23"),
            "a second call for the SAME day must not re-trigger the sweep"
        );
        assert!(
            !state.mark_swept_if_new_day("2026-07-23"),
            "repeated calls for the same day stay suppressed"
        );
        assert!(
            state.mark_swept_if_new_day("2026-07-24"),
            "a new day must trigger the sweep again"
        );
        assert!(
            !state.mark_swept_if_new_day("2026-07-24"),
            "the new day is cached in turn"
        );
    }
}
