//! Per-request freshness pipeline and post-apply increment commit.
//!
//! ### The per-request pipeline
//!
//! The pipeline is split across two entry points so it runs ONCE per request in
//! a fixed order, no matter which tool is calling. [`VaultContext::begin_request`]
//! runs steps 0–1 at the per-request seam (the server calls it before every tool
//! body — see `mcp::server`); [`VaultContext::query_cache`] runs steps 2–4 when a
//! tool actually opens the cache. Tools that reconstruct a graph index instead of
//! running a `query_cache` filter (validate, repair, set, edit, delete, move,
//! rewrite, apply, new) go through [`VaultContext::load_graph_index`], a thin
//! composition over `query_cache` plus the cache reader — so in warm mode those
//! tools bind the SAME generation and are served verify-once too, not cold-opened
//! per request (NRN-130). Putting root-liveness + config-freshness in
//! `begin_request` means *every* tool — query-cache and graph-index alike — gets
//! them, and config stays STABLE for the whole request (no mid-request swap, so
//! one request can never mix an old-config graph index with a new-config cache).
//!
//! 0. **Root liveness** (`begin_request`)**.** Canonicalize `vault_root`; if it is
//!    gone, return a typed [`WarmContextError::RootGone`] the daemon can downcast
//!    to evict the whole context.
//! 1. **Config freshness** (`begin_request`)**.** Read `<vault_root>/.norn/config.yaml`
//!    and compare a content-hash fingerprint (blake3 of the file bytes, plus
//!    `exists`). An existing-but-unreadable config (e.g. `chmod 000`) fails
//!    *this* request too, distinctly from "absent" — see [`fingerprint_config`].
//!    Unchanged → proceed. Changed → re-parse: a parse error fails *this* request
//!    (mirroring a direct CLI invocation) and leaves the fingerprint stale so the
//!    next request retries. On a successful re-parse the stored config `Arc` is
//!    swapped unconditionally; whether the change is index-relevant (and so needs
//!    a new generation) is decided in step 3 by comparing the bound generation's
//!    index identity against the new config — begin_request drops no state.
//! 2. **Ground-shift** (`ensure_current`, via `query_cache`)**.** Stat
//!    `<cache_dir>/cache.db` and compare its `(dev, ino)` against the identity the
//!    current generation captured at open. A missing file or a mismatch makes the
//!    generation stale. This catches an out-of-band `norn cache clear` / `prune` /
//!    manual `rm` under a live daemon: POSIX keeps an unlinked file alive through
//!    the held connection, so without this check a daemon would serve a ghost
//!    database forever.
//! 3. **(Re)open if stale** (`ensure_current`)**.** If the current generation is
//!    absent, invalidated (below the floor), ground-shifted, or opened under a
//!    now-different index identity, submit a generation-open writer-queue liveness
//!    op and block on it; the serialized op opens generation N+1 (or adopts one a
//!    concurrent op just produced). See the sentinel-discipline notes on
//!    `open_generation` for the ordering that keeps identity honest. This is the
//!    ONLY place `integrity_check` is paid in warm mode.
//! 4. **Freshness** (`query_cache_warm`)**.** Split into a request-thread PROBE
//!    and, only when it fails, a REFRESH (ADR 0013 Phase 2, NRN-253). The request
//!    takes the bound generation's READ guard and runs a
//!    [`FreshnessProbe`](crate::cache::FreshnessProbe) — today the read-only
//!    stat-sweep (`crate::cache::freshness`) — against it. **Fresh** → serve on
//!    that same guard, touching neither the writer queue nor the write
//!    connection, so a concurrent read of an unchanged vault costs one stat sweep,
//!    not a refresh op (before NRN-253 every request submitted one). **Stale** →
//!    drop the read guard and run the same lock-timeout-tolerant
//!    `index_incremental` refresh cold mode gets, as a **coalesced liveness op on
//!    the per-vault writer queue** (NRN-252), executed on the generation's WRITE
//!    connection and awaited before the read guard is re-taken and handed back.
//!    Arriving requesters that share a not-yet-started refresh coalesce onto one
//!    execution via a per-generation ticket; a requester is only ever satisfied by
//!    a refresh whose scan STARTED at or after its arrival. No re-probe follows
//!    the refresh — an arrival-correct refresh IS the freshness proof, the same
//!    trust semantics the always-refresh pipeline carried. `LockTimeout` still
//!    serves anyway with the NRN-215 both-surfaces note; any other error
//!    propagates with its concrete type intact (so corruption stays classifiable
//!    by [`note_tool_error`](VaultContext::note_tool_error)). The probe is the
//!    named interface a Phase 3 watcher-events impl slots behind; the stat sweep
//!    remains the permanent demoted-mode fallback. Warm mode RETIRES `call_lock`
//!    (NRN-253): tool bodies no longer serialize, so this probe/refresh
//!    concurrency is LIVE at the MCP surface — concurrent verified-fresh reads
//!    overlap on the read pool, and stale readers coalesce onto one refresh.
//!    (Cold stdio `norn mcp` keeps the lock as the NRN-55 cold-open guard.)
//!
//! ### Post-apply increment commit (NRN-252 / NRN-158)
//!
//! A warm MUTATION additionally commits its OWN cache increments after applying.
//! Each mutation tool feeds the changed-file set to
//! [`VaultContext::commit_apply_increments`], which parses the whole vault ONCE
//! **on the request thread** (no lock, off the writer thread — NRN-252 review)
//! into an `IncrementCommit`, then runs the commit as ONE **bulk** op on the
//! per-vault writer queue — a chunked closure that stages job-scoped document rows
//! and the full resolved links set in non-shadowing TEMP tables (~50ms chunks),
//! yields once more at ready-to-publish, then swaps all affected main rows in one
//! short transaction. A `still_valid` predicate drops the op if the generation
//! dies. Doing the parse on the request thread keeps every writer-queue chunk
//! bounded, so a liveness refresh queued behind the commit is not stalled O(parse).
//! The tool AWAITS it, so the report returns with the cache current.
//! Without this, the next read's freshness refresh would pay a full detect scan
//! AND a whole-vault rebuild (changes exist); with it, that refresh finds zero
//! changes. Failure is degraded, never propagated — the mutation already landed
//! on disk, so a deferred increment is healed by the next read (files are truth).

use super::*;

/// Content-hash fingerprint of `<vault_root>/.norn/config.yaml`, used by warm
/// mode to detect config edits between requests. A fingerprint for a
/// genuinely nonexistent file carries `exists = false` and `hash = None`. An
/// EXISTING-but-unreadable file (e.g. `chmod 000`) is NOT represented here —
/// [`fingerprint_config`] returns `Err` for that case instead, so it is never
/// silently conflated with "absent".
///
/// The fingerprint is a blake3 hash of the file's bytes — NOT `(mtime, size)`.
/// A stat-based fingerprint misses a same-size rewrite within mtime granularity
/// (e.g. an editor that preserves mtime, or two writes in the same clock tick),
/// silently diverging the warm daemon from a direct CLI run. Hashing the content
/// closes that hole; a few-KB read + blake3 per `begin_request` is negligible
/// against the 50ms query budget.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(in crate::env) struct ConfigFingerprint {
    exists: bool,
    /// blake3 of the file bytes; `None` when the file is absent.
    hash: Option<[u8; 32]>,
}

/// Fingerprint the config file at `path` by content. A genuinely missing file
/// yields `Ok` with `exists = false`. Any OTHER read failure (permission
/// denied, I/O error, ...) is a config that EXISTS but cannot be read, which
/// must not be conflated with "absent" — that would let the daemon keep
/// silently serving stale/default config while a direct CLI invocation on the
/// same vault fails. Such failures are returned as `Err` so they propagate out
/// of `begin_request` and fail that request, mirroring direct-CLI semantics
/// (same shape as the existing parse-error path: the fingerprint is left
/// stale so the next request retries).
///
/// We deliberately read + hash rather than stat: see [`ConfigFingerprint`]
/// for why mtime/size can't gate the read.
pub(in crate::env) fn fingerprint_config(path: &Utf8Path) -> Result<ConfigFingerprint> {
    match std::fs::read(path.as_std_path()) {
        Ok(bytes) => Ok(ConfigFingerprint {
            exists: true,
            hash: Some(*blake3::hash(&bytes).as_bytes()),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ConfigFingerprint {
            exists: false,
            hash: None,
        }),
        Err(error) => {
            Err(anyhow::Error::new(error).context(format!("failed to read config {path}")))
        }
    }
}

/// Marker strings a refresh ticket resolves to when its op never produced a
/// result — the queue dropped it on shutdown, or it panicked. Every waiter
/// surfaces these as an error (never a hang).
pub(in crate::env) const REFRESH_QUEUE_SHUTDOWN: &str =
    "warm writer queue is shutting down; freshness refresh abandoned";

pub(in crate::env) const REFRESH_PANICKED: &str =
    "warm writer queue panicked while running a freshness refresh";

/// Operator notes for a DEGRADED post-apply increment commit (NRN-252 / NRN-158).
/// The mutation already succeeded on disk, so these never fail the tool call —
/// they announce that the cache update was deferred and the next read's freshness
/// refresh will heal it (files remain the source of truth). Emitted on BOTH
/// surfaces: the daemon's own stderr and the per-request note buffer.
pub(in crate::env) const INCREMENT_FAILED_NOTE: &str = "norn serve: post-apply cache increment failed; the cache update was deferred and the next read's refresh will heal it";

pub(in crate::env) const INCREMENT_DROPPED_NOTE: &str = "norn serve: post-apply cache increment abandoned (generation evicted or queue shutdown); the next read's refresh will heal the cache";

pub(in crate::env) const INCREMENT_PANICKED_NOTE: &str =
    "norn serve: post-apply cache increment panicked; the next read's refresh will heal the cache";

/// A coalescing ticket for ONE freshness-refresh execution on a generation's
/// write connection (NRN-252). Requesters that arrive before the refresh op
/// starts share one ticket — and thus one `index_incremental` execution —
/// blocking here on the condvar rather than each queuing a redundant refresh.
///
/// The ticket resolves on EVERY path — success, `LockTimeout`, other error, and
/// (via the submitter's backstop) queue-drop / panic — so a waiter never hangs.
/// Every waiter observes the true outcome CLASS: a failed refresh is `Failed`
/// for all of them (the first takes the concrete `CacheError`, later waiters
/// synthesize one), a `LockTimeout` is `LockContention` for all of them, and a
/// clean refresh is `Served` for all of them — no coalesced waiter ever mistakes
/// a failed or contended refresh for a served one.
pub(in crate::env) struct RefreshTicket {
    state: Mutex<RefreshTicketState>,
    resolved: Condvar,
}

/// How a refresh op resolved, held on the shared ticket so EVERY coalesced
/// waiter reads the same outcome class. Making illegal states unrepresentable
/// (no `resolved` bool + `Option` result + `Option` reason triple) is what
/// prevents a later waiter from silently reading a failed refresh as served.
pub(in crate::env) enum Resolution {
    /// No outcome delivered yet; waiters block on the condvar.
    Pending,
    /// The op never produced a result (queue drop / panic) — every waiter fails.
    Abandoned(&'static str),
    /// The op ran and delivered a terminal class — every waiter sees this class.
    Done(DoneClass),
}

/// The terminal class of a completed refresh, shared across all coalesced
/// waiters.
pub(in crate::env) enum DoneClass {
    /// Clean refresh (or no changes) — `Served` for everyone.
    Ok,
    /// Timed out on the cache write lock — `LockContention` for everyone (each
    /// waiter emits its own both-surfaces note at its call site).
    LockTimeout,
    /// Non-timeout failure — `Failed` for everyone. The concrete `CacheError` is
    /// moved out by the FIRST waiter (so `note_tool_error` keeps classifying
    /// corruption on the untouched type); every later waiter synthesizes a
    /// [`CacheError::CoalescedRefreshFailed`] — it need not re-trigger corruption
    /// eviction, since the first waiter's propagation already bumped the floor
    /// and eviction is idempotent.
    Failed(Option<CacheError>),
}

pub(in crate::env) struct RefreshTicketState {
    /// Flipped true (under the generation's `refresh_pending` lock, so an
    /// arriving requester's started-check and join are atomic against it) when
    /// the op begins on the writer thread. A started refresh may have begun
    /// before a later arrival, so late arrivals never join it — they submit a
    /// fresh op whose scan is guaranteed to start after their arrival.
    started: bool,
    /// The single source of truth for this ticket's outcome. `Pending` until the
    /// op (or the submitter's drop/panic backstop) resolves it; the transition
    /// out of `Pending` happens once and is idempotent, and every waiter then
    /// reads the SAME class off it.
    resolution: Resolution,
}

impl RefreshTicket {
    fn new() -> Arc<Self> {
        Arc::new(RefreshTicket {
            state: Mutex::new(RefreshTicketState {
                started: false,
                resolution: Resolution::Pending,
            }),
            resolved: Condvar::new(),
        })
    }

    /// Deliver the op's `index_incremental` result. Idempotent: the first
    /// delivery wins (a later backstop resolve is a no-op). The concrete result
    /// is classified ONCE here into a [`DoneClass`] every waiter shares.
    fn resolve_completed(&self, result: Result<(), CacheError>) {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if !matches!(st.resolution, Resolution::Pending) {
            return;
        }
        let class = match result {
            Ok(()) => DoneClass::Ok,
            Err(CacheError::LockTimeout) => DoneClass::LockTimeout,
            Err(other) => DoneClass::Failed(Some(other)),
        };
        st.resolution = Resolution::Done(class);
        self.resolved.notify_all();
    }

    /// Resolve the ticket as abandoned (queue drop / panic). Idempotent.
    fn resolve_abandoned(&self, why: &'static str) {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if !matches!(st.resolution, Resolution::Pending) {
            return;
        }
        st.resolution = Resolution::Abandoned(why);
        self.resolved.notify_all();
    }

    /// Block until resolved, then classify the outcome for the requester. Every
    /// waiter observes the same CLASS: `Ok` → `Served`, `LockTimeout` →
    /// `LockContention`, `Failed` → `Failed` (first waiter gets the concrete
    /// error, later waiters a synthesized one), `Abandoned` → `Abandoned`.
    fn wait(&self) -> RefreshOutcome {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        while matches!(st.resolution, Resolution::Pending) {
            st = self.resolved.wait(st).unwrap_or_else(|p| p.into_inner());
        }
        match &mut st.resolution {
            Resolution::Pending => unreachable!("loop exits only once resolved"),
            Resolution::Abandoned(why) => RefreshOutcome::Abandoned(why),
            Resolution::Done(DoneClass::Ok) => RefreshOutcome::Served,
            Resolution::Done(DoneClass::LockTimeout) => RefreshOutcome::LockContention,
            // First waiter takes the concrete error (corruption stays
            // classifiable); every later coalesced waiter still fails, with a
            // synthesized error — none ever reads this as served.
            Resolution::Done(DoneClass::Failed(err)) => {
                RefreshOutcome::Failed(err.take().unwrap_or(CacheError::CoalescedRefreshFailed))
            }
        }
    }

    pub(in crate::env) fn is_started(&self) -> bool {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).started
    }

    fn mark_started(&self) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).started = true;
    }
}

/// How a coalesced freshness refresh resolved, as seen by a requester.
pub(in crate::env) enum RefreshOutcome {
    /// The refresh completed cleanly (or nothing changed) — serve the read.
    Served,
    /// The refresh timed out on the cache write lock. Serve anyway, emitting the
    /// NRN-215 both-surfaces contention note.
    LockContention,
    /// The refresh failed with a non-timeout error; propagate it (its concrete
    /// `CacheError` is intact for corruption classification).
    Failed(CacheError),
    /// The op was dropped (queue shutdown) or panicked; fail this request.
    Abandoned(&'static str),
}

/// The result of arriving at a generation's coalesced refresh: either this
/// caller SUBMITTED the op (and holds the queue handle that backstops
/// drop/panic) or it JOINED an already-pending, not-yet-started op.
pub(in crate::env) enum RefreshArrival {
    Submitted {
        ticket: Arc<RefreshTicket>,
        handle: Handle<()>,
    },
    Joined {
        ticket: Arc<RefreshTicket>,
    },
}

impl RefreshArrival {
    /// Block until the refresh resolves. The submitter waits on the queue handle
    /// first — so a dropped (shutdown) or panicked op still resolves the ticket
    /// rather than hanging every joined waiter — then reads the ticket; a joiner
    /// simply blocks on the shared ticket.
    pub(in crate::env) fn wait(self, generation: &Generation) -> RefreshOutcome {
        match self {
            RefreshArrival::Joined { ticket } => ticket.wait(),
            RefreshArrival::Submitted { ticket, handle } => {
                match handle.wait() {
                    Outcome::Done(()) => {}
                    Outcome::Dropped => {
                        ticket.resolve_abandoned(REFRESH_QUEUE_SHUTDOWN);
                        clear_pending_if(generation, &ticket);
                    }
                    Outcome::Panicked => {
                        ticket.resolve_abandoned(REFRESH_PANICKED);
                        clear_pending_if(generation, &ticket);
                    }
                }
                ticket.wait()
            }
        }
    }

    /// Test accessor: the ticket this arrival is bound to.
    #[cfg(test)]
    pub(in crate::env) fn ticket(&self) -> &Arc<RefreshTicket> {
        match self {
            RefreshArrival::Submitted { ticket, .. } | RefreshArrival::Joined { ticket } => ticket,
        }
    }
}

/// Test-only handshake gate installed on a generation to hold a writer-queue op
/// at a control point under test control (no sleeps). One shape serves both uses:
///
/// - the freshness-refresh gate (`refresh_gate`) is ONE-SHOT — the refresh op
///   waits on it once, AFTER its start transition (started flag set, pending
///   cleared) and BEFORE its scan, so a test can hold a refresh "in flight" while
///   a new requester arrives;
/// - the increment-commit gate (`increment_gate`) is REUSABLE — the commit op
///   hits it at EVERY chunk boundary (after TEMP staging, before the next), so
///   a test can step a multi-chunk commit one boundary at a time.
///
/// In both: `reached.recv()` unblocks when the op reaches the gate; the test
/// inspects state, then `release.send(())` lets the op proceed.
#[cfg(test)]
pub(in crate::env) struct TestGate {
    /// Signalled by the op once it reaches the gate.
    pub(in crate::env) reached: std::sync::mpsc::Sender<()>,
    /// The op blocks receiving here until the test releases it.
    pub(in crate::env) release: std::sync::mpsc::Receiver<()>,
}

/// Clear a generation's pending-refresh slot IFF it still points at `ticket`.
/// Used by the submitter's drop/panic backstop so a superseded ticket never
/// lingers in the slot.
pub(in crate::env) fn clear_pending_if(generation: &Generation, ticket: &Arc<RefreshTicket>) {
    let mut pending = generation
        .refresh_pending
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if pending.as_ref().is_some_and(|p| Arc::ptr_eq(p, ticket)) {
        *pending = None;
    }
}

/// The body of a freshness-refresh liveness op, run serialized on the writer
/// thread against the generation's WRITE connection (NRN-252).
///
/// It first performs the START TRANSITION under the generation's
/// `refresh_pending` lock — marking the ticket started and removing it from the
/// pending slot — so a concurrently-arriving requester either joins this ticket
/// BEFORE the transition (its arrival precedes this scan) or, seeing the slot
/// cleared, submits a fresh op AFTER (its arrival precedes THAT scan). Either
/// way a requester is only ever served by a refresh whose scan starts at or
/// after its arrival. It then runs `index_incremental` and resolves the ticket
/// with the concrete result.
pub(in crate::env) fn run_refresh_op(
    generation: &Generation,
    vault_root: &Utf8Path,
    ticket: &Arc<RefreshTicket>,
) {
    // Start transition: mark started and clear the pending slot atomically under
    // the pending lock (lock order: pending → ticket state, matching the arrival
    // path, so the join decision cannot straddle it).
    {
        let mut pending = generation
            .refresh_pending
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        ticket.mark_started();
        if pending.as_ref().is_some_and(|p| Arc::ptr_eq(p, ticket)) {
            *pending = None;
        }
    }

    // Test-only: block "in flight" between the start transition and the scan.
    #[cfg(test)]
    {
        let gate = generation
            .refresh_gate
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(gate) = gate {
            let _ = gate.reached.send(());
            let _ = gate.release.recv();
        }
    }

    // Test-only: return an injected error instead of scanning, to drive a
    // refresh failure (e.g. corruption) through the ticket deterministically.
    #[cfg(test)]
    {
        let injected = generation
            .inject_refresh_error
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(err) = injected {
            ticket.resolve_completed(Err(err));
            return;
        }
    }

    #[cfg(test)]
    generation
        .refresh_exec_count
        .fetch_add(1, Ordering::Relaxed);

    let result = {
        let mut write_cache = generation
            .write_cache
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let report = write_cache.index_incremental(vault_root, &ChangeDetectOptions::default());
        if report.is_ok() {
            write_cache.supersede_staged_increments_after_refresh();
        }
        // Test-only: capture the report so the NRN-158 acceptance test can assert
        // it is empty (zero changes ⇒ no whole-vault rebuild) after an increment.
        #[cfg(test)]
        if let Ok(r) = &report {
            *generation
                .last_refresh_report
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = Some(r.clone());
        }
        report.map(|_report| ())
    };
    ticket.resolve_completed(result);
}

/// The body of a post-apply cache-increment commit BULK op, run serialized on the
/// writer thread against the generation's WRITE connection (ADR 0013 Phase 2,
/// NRN-252 / NRN-158).
///
/// It is a chunked closure driving an ALREADY-PARSED [`IncrementCommit`] (the
/// whole-vault parse ran on the caller thread, off the writer thread — NRN-252
/// review): bulk invocations stage one bounded chunk in connection-private TEMP;
/// the terminal invocation alone acquires the WriteLock and publishes all main
/// rows atomically. External CLI processes and this daemon's liveness ops can
/// interleave at every staging boundary, including immediately before publish.
/// Returns [`ChunkOutcome::More`] while work remains, [`ChunkOutcome::Done`] with
/// the terminal `Result` on completion or the first chunk error.
pub(in crate::env) fn run_increment_chunk(
    generation: &Generation,
    vault_root: &Utf8Path,
    driver: &mut crate::cache::IncrementCommit,
    budget: std::time::Duration,
) -> ChunkOutcome<Result<(), CacheError>> {
    #[cfg(test)]
    if generation
        .inject_increment_panic
        .swap(false, Ordering::AcqRel)
    {
        panic!("injected increment panic");
    }
    // Test-only: fail this chunk with an injected error instead of committing,
    // to drive an increment failure (e.g. corruption) deterministically.
    #[cfg(test)]
    {
        let injected = generation
            .inject_increment_error
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(err) = injected {
            return ChunkOutcome::Done(Err(err));
        }
    }

    let mut write_cache = generation
        .write_cache
        .lock()
        .unwrap_or_else(|p| p.into_inner());

    let outcome = match write_cache.commit_increment_chunk(vault_root, driver, budget) {
        Ok(true) => ChunkOutcome::More,
        Ok(false) => ChunkOutcome::Done(Ok(())),
        Err(e) => ChunkOutcome::Done(Err(e)),
    };
    // Release the write connection BEFORE hitting the test gate, so a test can
    // inspect the read connection while the op is parked at the boundary.
    drop(write_cache);

    // Test-only: park at each chunk boundary (a chunk just committed) so a test
    // can step the commit deterministically. Never fires when no gate installed.
    #[cfg(test)]
    if matches!(outcome, ChunkOutcome::More) {
        let gate = generation
            .increment_gate
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(g) = gate.as_ref() {
            let _ = g.reached.send(());
            let _ = g.release.recv();
        }
    }

    outcome
}

/// Path to a vault's default config file, `<vault_root>/.norn/config.yaml`.
pub(in crate::env) fn config_yaml_path(vault_root: &Utf8Path) -> Utf8PathBuf {
    vault_root.join(".norn/config.yaml")
}

impl VaultContext {
    /// Per-request entry seam (FIX-1): runs the warm pipeline's root-liveness
    /// (step 0) and config-freshness (step 1) once per request, BEFORE the tool
    /// body reads `config()` or opens the cache. No-op in cold mode (config is
    /// fixed for the process lifetime and the cache is opened fresh each call).
    ///
    /// Split out of `query_cache` so every tool — including the ones that bypass
    /// `query_cache` and build the graph index via `load_graph_index` — gets
    /// root-liveness + a fresh config each request, and so config is STABLE for
    /// the whole request: `query_cache` no longer swaps config mid-request,
    /// closing the split-brain window where one request could mix an old-config
    /// graph index with a new-config cache. A gone root surfaces here as the
    /// typed [`WarmContextError::RootGone`] the daemon downcasts to evict.
    ///
    /// Returns the request's [`RequestScope`] (NRN-253): a fresh, empty note
    /// buffer, no generation bound yet, and the config `Arc` bound at this
    /// boundary — the tool body threads it and `run_wrapped` drains its notes.
    /// A fresh scope per request is what makes per-request isolation structural:
    /// there is no shared buffer or slot cell to clear, so a prior request's
    /// notes / generation attribution cannot leak into this one.
    pub(crate) fn begin_request(&self) -> Result<RequestScope> {
        let Mode::Warm(slot) = &self.mode else {
            // Cold mode: config is fixed for the process lifetime, so the bound
            // config is simply the current one. No liveness/freshness steps.
            return Ok(RequestScope::new(self.config()));
        };

        // Step 0 — root liveness. A gone root is a typed, downcast-matchable
        // error so the daemon can evict this whole context.
        std::fs::canonicalize(self.vault_root.as_std_path()).map_err(|source| {
            anyhow::Error::new(WarmContextError::RootGone {
                root: self.vault_root.clone(),
                source,
            })
        })?;

        // Step 1 — config freshness. Swaps the stored config `Arc` on a change and
        // advances the fingerprint; a parse error fails this request without
        // advancing it. begin_request drops NO generation state: whether the
        // change is index-relevant (and so needs a new generation) is decided in
        // `ensure_current` by comparing the bound generation's index identity to
        // the new config, so a single path owns every reopen.
        self.refresh_config_warm(slot)?;

        // Bind the now-current config into the request scope, AFTER the freshness
        // swap, so the whole request reads one stable config `Arc` even if a later
        // concurrent request swaps the stored config out from under it (NRN-251).
        Ok(RequestScope::new(self.config()))
    }

    /// The warm per-request pipeline (steps 2–4). See the module-level docs for
    /// the ordered rationale of each step.
    pub(in crate::env) fn query_cache_warm(
        &self,
        slot: &WarmSlot,
        scope: &RequestScope,
    ) -> Result<CacheHandle> {
        // Steps 0–1 (root-liveness + config-freshness) already ran in
        // `begin_request` at the per-request seam, so config is stable here for
        // the whole request. Steps 2–3 (ground-shift + reopen-if-stale) are the
        // single-flight `ensure_current`, which returns the generation this
        // request binds for its whole body.
        let generation = self.ensure_current(slot, scope)?;

        // Record which generation THIS request bound into its OWN scope, before
        // doing any further work that could fail (the freshness refresh below, or
        // the tool body after this returns) — so a corruption error noted anywhere
        // downstream attributes to the generation this request actually used, not
        // whatever is current by the time `note_tool_error` runs (NRN-253).
        scope.bind_generation(generation.number);

        // Step 4 — freshness, split into PROBE then (only if stale) REFRESH
        // (NRN-253). Check out an owned READ connection from the bound generation's
        // pool, held across the whole (sync) tool body. It outlives a concurrent
        // generation swap of the slot pointer because the checkout owns its own
        // `Arc<ReadPool>` and the handle also holds the `Arc<Generation>` (keeping
        // the sentinel alive). Every pooled connection is `query_only`, so this
        // connection cannot write; the freshness refresh writes through the WRITE
        // connection instead.
        let read_conn = generation.checkout_read();

        // Probe on THIS request thread against the pooled connection. Fresh → serve
        // on this very connection, touching neither the writer queue nor the write
        // connection: a concurrent read of an unchanged vault costs one stat
        // sweep, not a refresh op. Stale (or a probe error — treated
        // conservatively as stale, since the refresh is authoritative and would
        // re-surface any real fault) → route through the coalesced refresh. No
        // re-probe after: an arrival-correct refresh IS the freshness proof, the
        // same trust semantics the always-refresh pipeline carried.
        let fresh = matches!(
            StatSweepProbe.probe(&self.vault_root, &read_conn),
            Ok(Freshness::Fresh)
        );

        if !fresh {
            // HOLD the pooled connection across the refresh wait (NRN-253), rather
            // than releasing and re-checking-out. It is sound and simpler: the
            // refresh runs on the generation's WRITE connection, so a held READ
            // connection never contends with it (WAL makes the refresh's committed
            // rows visible to this connection's next query — the probe finalized its
            // statement, so this connection holds no read transaction pinning an
            // older snapshot); and because reads come from a POOL, holding one
            // connection does not serialize other concurrent readers the way holding
            // the pre-NRN-253 single mutex would have.
            match self.refresh_generation(slot, &generation) {
                RefreshOutcome::Served => {}
                RefreshOutcome::LockContention => {
                    // BOTH surfaces (NRN-215): the daemon's own stderr is its
                    // operational log — an operator tailing `norn serve` (or a
                    // log pipeline) keeps the contention signal, alongside the
                    // served markers — AND the per-request note buffer carries
                    // it to the caller: `run_wrapped` forwards it in the tool
                    // envelope and the routed CLI re-emits it on ITS stderr,
                    // byte-identical to a direct run.
                    self.note_both_surfaces(scope, crate::cache::LOCK_CONTENTION_NOTE);
                }
                // The concrete `CacheError` survives the ticket, so a
                // corruption-class failure remains classifiable by
                // `note_tool_error` downstream.
                RefreshOutcome::Failed(error) => return Err(error.into()),
                RefreshOutcome::Abandoned(msg) => return Err(anyhow::anyhow!(msg)),
            }
        }

        Ok(CacheHandle::Warm(WarmGuard {
            floor: Arc::clone(&slot.shared.floor),
            generation,
            conn: read_conn,
        }))
    }

    /// Run the coalesced freshness refresh for `generation` and block on its
    /// outcome (NRN-252). A thin composition: [`arrive_refresh`](Self::arrive_refresh)
    /// decides join-or-submit, then [`RefreshArrival::wait`] blocks with the
    /// drop/panic backstop.
    fn refresh_generation(&self, slot: &WarmSlot, generation: &Arc<Generation>) -> RefreshOutcome {
        self.arrive_refresh(slot, generation).wait(generation)
    }

    /// Arrive at `generation`'s coalesced refresh WITHOUT blocking. Under the
    /// generation's `refresh_pending` lock (so the started-check and the join are
    /// atomic against an op's start transition), either join a pending,
    /// not-yet-started refresh or submit a fresh writer-queue liveness op and
    /// install it as the pending one. Returns the [`RefreshArrival`] the caller
    /// then blocks on.
    fn arrive_refresh(&self, slot: &WarmSlot, generation: &Arc<Generation>) -> RefreshArrival {
        let decision = {
            let mut pending = generation
                .refresh_pending
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            // Test-only: count this arrival UNDER the pending lock, so it reflects a
            // FINALIZED join-or-submit decision. A full-pipeline test waits until
            // BOTH stale readers have arrived (still under a writer-queue blocker, so
            // neither op has started) before releasing them — with the count taken
            // here, `arrivals == 2` guarantees the second reader has already joined
            // the first's ticket and cannot instead submit a second op (NRN-253).
            #[cfg(test)]
            generation.refresh_arrivals.fetch_add(1, Ordering::Relaxed);
            // Join only a pending refresh that has NOT started: its eventual scan
            // is then necessarily after this arrival. (An already-started op has
            // cleared itself from the slot, so this normally sees `None` there;
            // the started re-check is a belt-and-suspenders guard on the race.)
            let joinable = pending.as_ref().and_then(|t| {
                if t.is_started() {
                    None
                } else {
                    Some(Arc::clone(t))
                }
            });
            match joinable {
                Some(ticket) => Some(ticket),
                None => {
                    let ticket = RefreshTicket::new();
                    *pending = Some(Arc::clone(&ticket));
                    // Signal "submit" by returning None here and building the op
                    // below with this freshly-installed ticket.
                    return self.submit_refresh(slot, generation, ticket);
                }
            }
        };
        RefreshArrival::Joined {
            ticket: decision.expect("join path returns Some"),
        }
    }

    /// Build and submit a freshness-refresh liveness op for `ticket`, returning
    /// the [`RefreshArrival::Submitted`] handle. The op runs `'static` on the
    /// writer thread, so it captures owned / `Arc` state — never `&self`/`&slot`.
    fn submit_refresh(
        &self,
        slot: &WarmSlot,
        generation: &Arc<Generation>,
        ticket: Arc<RefreshTicket>,
    ) -> RefreshArrival {
        let gen_op = Arc::clone(generation);
        let ticket_op = Arc::clone(&ticket);
        let vault_root = self.vault_root.clone();
        let handle = slot
            .queue
            .submit_liveness(move || run_refresh_op(&gen_op, &vault_root, &ticket_op));
        RefreshArrival::Submitted { ticket, handle }
    }

    /// Commit the changed-file set from a just-succeeded WARM mutation as a
    /// CHUNKED cache-increment bulk op on the per-vault writer queue, AWAITED
    /// before returning (ADR 0013 Phase 2, NRN-252 / NRN-158). The tool returns
    /// with the cache already current, so the NEXT read's freshness refresh finds
    /// zero changes and pays neither a full detect scan nor a whole-vault rebuild.
    /// No-op in cold mode (no queue) and for an empty set.
    ///
    /// **Failure is degraded, never propagated.** The mutation ALREADY succeeded
    /// on disk, so an increment `Err` / `Dropped` / `Panicked` only means the
    /// cache update was deferred — the next read's refresh detects the change and
    /// heals it, with the files still the source of truth (trust preserved). The
    /// degrade emits the both-surfaces operator note; a corruption-class error
    /// still evicts + re-verifies on the next request via
    /// [`evict_generation_on_corruption`](Self::evict_generation_on_corruption),
    /// keyed off the generation the increment RAN ON (the `slot.current` this
    /// method binds below) — NOT the scope's bound generation, which can lag it
    /// by a concurrent reopen and would leave the actually-corrupt newer
    /// generation serving.
    pub(crate) fn commit_apply_increments(
        &self,
        scope: &RequestScope,
        changed_paths: &[Utf8PathBuf],
        baseline: crate::core::GraphIndex,
    ) {
        let Mode::Warm(slot) = &self.mode else {
            return; // cold: no queue, nothing to commit
        };
        if changed_paths.is_empty() {
            return;
        }
        // The generation the request is committing against. Read from the slot's
        // current pointer; a concurrent reopen only ever swaps it forward, and the
        // bulk op's `still_valid` guard drops the commit if this generation dies
        // mid-flight, so binding to `current` here is safe post-`call_lock`
        // (NRN-253). If none is current, there is nothing to update — the next open
        // covers it.
        let Some(generation) = slot
            .shared
            .current
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        else {
            return;
        };
        if scope.bound_generation() != generation.number {
            self.note_both_surfaces(scope, INCREMENT_DROPPED_NOTE);
            return;
        }
        let baseline_fingerprint = crate::cache::graph_fingerprint(&baseline);

        // Reserve publication authority on the dedicated writer connection
        // BEFORE parsing. A refresh can now supersede this job even during the
        // parse/pre-first-chunk window, and an external publication changes the
        // baseline captured by the reservation.
        let reservation = match self
            .submit_increment_reservation(slot, &generation, baseline_fingerprint)
            .wait()
        {
            Outcome::Done(Ok(reservation)) => reservation,
            Outcome::Done(Err(error)) => {
                let err: anyhow::Error = error.into();
                self.evict_generation_on_corruption(generation.number, &err);
                self.note_both_surfaces(scope, INCREMENT_FAILED_NOTE);
                return;
            }
            Outcome::Dropped => {
                self.note_both_surfaces(scope, INCREMENT_DROPPED_NOTE);
                return;
            }
            Outcome::Panicked => {
                self.note_both_surfaces(scope, INCREMENT_PANICKED_NOTE);
                return;
            }
        };

        // Build the whole-vault parse (the IncrementCommit driver) HERE, on the
        // request's spawn_blocking thread (post-apply, under the mutation lock),
        // NOT on the writer thread (NRN-252 review). The parse is O(vault) and
        // was previously the bulk op's unbounded, non-preemptible first "chunk" —
        // defeating the ~50ms preemption bound a liveness refresh queued behind it
        // relies on. It is read-only over post-apply disk state and needs only the
        // generation's index config (alias_field / files.ignore), no cache
        // connection, so it belongs off the writer thread. The caller supplies
        // the coherent pre-apply graph it already used for planning; reusing it
        // avoids both duplicate O(vault) reconstruction and a second read-pool
        // checkout while that mutation may still own the sole pooled handle.
        let commit = match crate::cache::Cache::begin_increment_commit(
            &self.vault_root,
            changed_paths,
            generation.index_identity.alias_field.as_deref(),
            &generation.index_identity.ignore,
            &reservation,
            baseline,
        ) {
            Ok(commit) => commit,
            Err(error) => {
                self.discard_increment_reservation(slot, &generation, reservation);
                // The parse failed, but the mutation ALREADY landed on disk — so
                // degrade rather than fail the tool: the next read's refresh heals
                // the cache. A corruption-class parse error still evicts +
                // re-verifies — keyed off the generation this commit targets, the
                // same key the post-run arm below uses.
                let err: anyhow::Error = error.into();
                self.evict_generation_on_corruption(generation.number, &err);
                self.note_both_surfaces(scope, INCREMENT_FAILED_NOTE);
                return;
            }
        };

        let outcome = self
            .submit_increment_commit(slot, &generation, commit)
            .wait();
        match outcome {
            Outcome::Done(Ok(())) => {}
            Outcome::Done(Err(error)) => {
                // A corruption-class error must still evict + re-verify on the
                // next request — keyed off the generation the increment RAN ON
                // (`generation`, bound from `slot.current` above), NOT the
                // scope's bound generation: after a concurrent reopen the scope
                // can still point at older N while this increment corrupted on
                // N+1, and a bound-keyed bump (floor = N+1) would leave the
                // corrupt N+1 satisfying the floor and serving (NRN-253 review).
                let err: anyhow::Error = error.into();
                self.evict_generation_on_corruption(generation.number, &err);
                self.note_both_surfaces(scope, INCREMENT_FAILED_NOTE);
            }
            Outcome::Dropped => {
                self.discard_increment_reservation(slot, &generation, reservation);
                self.note_both_surfaces(scope, INCREMENT_DROPPED_NOTE);
            }
            Outcome::Panicked => {
                self.discard_increment_reservation(slot, &generation, reservation);
                self.note_both_surfaces(scope, INCREMENT_PANICKED_NOTE);
            }
        }
    }

    pub(in crate::env) fn submit_increment_reservation(
        &self,
        slot: &WarmSlot,
        generation: &Arc<Generation>,
        expected_fingerprint: String,
    ) -> Handle<Result<crate::cache::IncrementReservation, CacheError>> {
        let generation = Arc::clone(generation);
        slot.queue.submit_liveness(move || {
            generation
                .write_cache
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .reserve_increment_commit(&expected_fingerprint)
        })
    }

    fn discard_increment_reservation(
        &self,
        slot: &WarmSlot,
        generation: &Arc<Generation>,
        reservation: crate::cache::IncrementReservation,
    ) {
        let generation = Arc::clone(generation);
        let _ = slot
            .queue
            .submit_liveness(move || {
                generation
                    .write_cache
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .discard_increment_reservation(&reservation)
            })
            .wait();
    }

    /// Emit an operator note on BOTH surfaces (NRN-215 pattern): the daemon's own
    /// stderr keeps the signal for an operator tailing `norn serve`, and the
    /// per-request note buffer carries it to the caller (forwarded by
    /// `run_wrapped` and re-emitted on the routed CLI's stderr). The ONE shared
    /// shape for every both-surfaces note — the post-apply increment-deferred
    /// degrade notes and the freshness-refresh lock-contention note alike.
    fn note_both_surfaces(&self, scope: &RequestScope, note: &str) {
        eprintln!("{note}");
        scope.push_operator_note(note);
    }

    /// Submit the increment-commit bulk op for `generation`, driving the ALREADY
    /// PARSED `commit` chunk by chunk, and return its queue handle. The op runs
    /// `'static` on the writer thread, so it captures owned / `Arc` state; the
    /// whole-vault parse already happened on the caller thread (NRN-252 review),
    /// so the op only stages bounded TEMP chunks before one terminal atomic
    /// publication — every staging chunk preemptible by a liveness op at its
    /// boundary.
    pub(in crate::env) fn submit_increment_commit(
        &self,
        slot: &WarmSlot,
        generation: &Arc<Generation>,
        commit: crate::cache::IncrementCommit,
    ) -> Handle<Result<(), CacheError>> {
        let gen_op = Arc::clone(generation);
        let vault_root = self.vault_root.clone();
        let budget = crate::cache::increment_chunk_budget();
        // The prebuilt chunk-driver, moved into the op and advanced one chunk per
        // invocation.
        let mut driver = commit;
        let step = move || run_increment_chunk(&gen_op, &vault_root, &mut driver, budget);
        let still_valid = generation_still_current_guard(&slot.shared, generation.number);
        slot.queue.submit_bulk(step, Some(still_valid))
    }

    /// Step 1 body. On a config-file change, re-parses and swaps the stored config
    /// `Arc` (advancing the fingerprint); an index-relevant change is NOT acted on
    /// here — `ensure_current` reopens when the bound generation's index identity
    /// no longer matches. On no change, touches nothing. On a parse error, returns
    /// `Err` WITHOUT advancing the fingerprint (so the next request retries),
    /// mirroring a direct CLI invocation.
    fn refresh_config_warm(&self, slot: &WarmSlot) -> Result<()> {
        let config_path = config_yaml_path(&self.vault_root);
        let new_fp = fingerprint_config(&config_path)?;

        let mut fp_guard = slot.config_fp.lock().unwrap_or_else(|p| p.into_inner());
        if *fp_guard == new_fp {
            return Ok(());
        }

        // Changed — re-parse. A parse error propagates and the fingerprint stays
        // stale, mirroring what a direct CLI invocation would do on this vault.
        let new_config = load_config(&self.vault_root.to_path_buf(), None)?;

        {
            let mut cfg = self.config.lock().unwrap_or_else(|p| p.into_inner());
            *cfg = Arc::new(new_config);
        }
        *fp_guard = new_fp;

        Ok(())
    }

    /// Test-only: freshness-refresh executions on the CURRENT generation (`0` if
    /// none). The `server`-module coalescing proof reads its delta to assert two
    /// concurrent stale readers shared EXACTLY ONE refresh execution (NRN-253).
    #[cfg(test)]
    pub(crate) fn current_refresh_exec_count(&self) -> u64 {
        self.current_generation()
            .map(|g| g.refresh_exec_count.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Test-only: arrivals at the CURRENT generation's coalesced refresh (`0` if
    /// none). Lets a full-pipeline test wait until both stale readers have arrived
    /// before releasing the writer-queue blocker they coalesce behind (NRN-253).
    #[cfg(test)]
    pub(crate) fn current_refresh_arrivals(&self) -> u64 {
        self.current_generation()
            .map(|g| g.refresh_arrivals.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Test-only: gate the current generation's next freshness refresh after its
    /// start transition but before its scan. Returns the test's
    /// `(refresh_started_rx, release_refresh_tx)` ends so an MCP-surface proof can
    /// deterministically introduce a later request while that refresh is in flight.
    #[cfg(test)]
    pub(crate) fn install_current_refresh_gate(
        &self,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let generation = self.current_generation().expect("a current generation");
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        *generation
            .refresh_gate
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(TestGate {
            reached: reached_tx,
            release: release_rx,
        });
        (reached_rx, release_tx)
    }

    /// Test-only: arrive at `generation`'s coalesced refresh WITHOUT blocking,
    /// so a test can sequence multiple arrivals against a blocked writer and
    /// inspect the resulting join-or-submit decision.
    #[cfg(test)]
    pub(in crate::env) fn test_arrive_refresh(
        &self,
        generation: &Arc<Generation>,
    ) -> RefreshArrival {
        match &self.mode {
            Mode::Warm(slot) => self.arrive_refresh(slot, generation),
            Mode::Cold => panic!("test_arrive_refresh called on a cold context"),
        }
    }

    /// Test-only: run one coalesced freshness refresh against the CURRENT
    /// generation and block on its outcome — the full arrive-then-wait path.
    #[cfg(test)]
    pub(in crate::env) fn test_refresh_current(&self) -> RefreshOutcome {
        match &self.mode {
            Mode::Warm(slot) => {
                let generation = slot
                    .shared
                    .current
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone()
                    .expect("a current generation");
                self.refresh_generation(slot, &generation)
            }
            Mode::Cold => panic!("test_refresh_current called on a cold context"),
        }
    }

    /// Test-only: how many changes a `detect` scan reports against the CURRENT
    /// generation's READ connection. The NRN-158 acceptance test asserts 0 after a
    /// warm mutation committed its increment (⇒ the next refresh does no
    /// whole-vault rebuild).
    #[cfg(test)]
    pub(crate) fn detect_change_count(&self) -> usize {
        let generation = self.current_generation().expect("a current generation");
        let cache = generation.test_read_conn();
        cache.detect_change_count(&self.vault_root)
    }

    /// Test-only: the `IndexReport` the current generation's most recent freshness
    /// refresh produced (`None` if none has run). An empty report (all-zero
    /// counts) means the refresh detected zero changes and did not rebuild.
    #[cfg(test)]
    pub(crate) fn last_refresh_report(&self) -> Option<crate::cache::IndexReport> {
        self.current_generation().and_then(|g| {
            g.last_refresh_report
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        })
    }

    /// Test-only: install a per-chunk gate on `generation`'s increment op and hand
    /// back the test's `(reached_rx, release_tx)` ends. The op signals `reached`
    /// after each TEMP chunk (and at ready-to-publish) and blocks on `release`, so a test steps a
    /// multi-chunk commit one boundary at a time.
    #[cfg(test)]
    pub(crate) fn install_increment_gate(
        &self,
        generation: &Arc<Generation>,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        *generation
            .increment_gate
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(TestGate {
            reached: reached_tx,
            release: release_rx,
        });
        (reached_rx, release_tx)
    }

    /// Test-only: submit an increment-commit bulk op for `generation` exactly as
    /// [`commit_apply_increments`](Self::commit_apply_increments) does, returning
    /// the raw queue handle (not awaited), so a test can drive the gate and
    /// inspect intermediate state before the op resolves.
    #[cfg(test)]
    pub(crate) fn test_submit_increment_commit(
        &self,
        generation: &Arc<Generation>,
        changed_paths: &[Utf8PathBuf],
    ) -> Handle<Result<(), CacheError>> {
        match &self.mode {
            Mode::Warm(slot) => {
                // Mirror `commit_apply_increments`: reserve on the writer, parse
                // on THIS (test) thread, then submit the prebuilt driver.
                let baseline = generation
                    .checkout_read()
                    .load_graph_index()
                    .expect("test baseline load should succeed");
                let fingerprint = crate::cache::graph_fingerprint(&baseline);
                let reservation = match self
                    .submit_increment_reservation(slot, generation, fingerprint)
                    .wait()
                {
                    Outcome::Done(Ok(reservation)) => reservation,
                    other => panic!("test increment reservation failed: {other:?}"),
                };
                let commit = crate::cache::Cache::begin_increment_commit(
                    &self.vault_root,
                    changed_paths,
                    generation.index_identity.alias_field.as_deref(),
                    &generation.index_identity.ignore,
                    &reservation,
                    baseline,
                )
                .expect("test increment parse should succeed");
                self.submit_increment_commit(slot, generation, commit)
            }
            Mode::Cold => panic!("test_submit_increment_commit called on a cold context"),
        }
    }
}
