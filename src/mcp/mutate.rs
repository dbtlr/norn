//! Shared mutation plumbing for the MCP mutation tools.
//!
//! The MCP mutation contract (`vault.set` today; `vault.new` / `vault.move` /
//! `vault.delete` / `vault.apply` next) must produce the SAME append-only
//! event-stream records a CLI mutation does — that audit trail is how an
//! off-filesystem MCP client gets "audited for free." This module owns the one
//! seam that guarantees it: [`open_mutation_event_sink`], the MCP analogue of
//! `main.rs::open_event_sink`'s real-apply branch.
//!
//! Keeping it here (not duplicated per tool) means every later mutation tool
//! inherits correct auditing by calling the same helper with the same semantics:
//! honor `telemetry.location`, prune/size-cap before opening, and fall back to a
//! `discard` sink if anything about opening the file fails — telemetry must
//! never block or fail a mutation.

use crate::mcp::context::VaultContext;
use crate::mutation_lock::MutationLock;
use crate::telemetry::{Clock, EventSink, IdGen};
use camino::Utf8Path;

/// Acquire the per-vault mutation lock on an MCP mutation's CONFIRM/apply path.
///
/// Sweeps stale pending markers, then acquires the lock with `is_apply = true` —
/// every MCP mutation reaches this only after its dry-run early-return, so the
/// apply is always real. Returns the RAII guard the caller must hold for the
/// duration of the apply; a timeout or lock error becomes an `anyhow` error.
///
/// This is the MCP analogue of the CLI's lock block in `main.rs`, which differs
/// deliberately: the CLI maps a timeout to exit code 2 + a stderr line, whereas
/// an MCP tool surfaces it as a tool error.
pub(crate) fn acquire_mutation_lock(cwd: &Utf8Path) -> anyhow::Result<Option<MutationLock>> {
    let (_, state_dir) = crate::cache::state_dir_for(cwd)
        .map_err(|e| anyhow::anyhow!("could not resolve state dir: {e}"))?;
    crate::mutation_lock::pending::sweep_pending(&state_dir);
    match MutationLock::acquire_if_mutating(&state_dir, /*is_apply=*/ true) {
        Ok(guard) => Ok(guard),
        Err(crate::cache::CacheError::MutationLockTimeout) => anyhow::bail!(
            "another norn mutation is in progress against this vault (timed out after 5 s)"
        ),
        Err(e) => anyhow::bail!("mutation lock error: {e}"),
    }
}

/// Open a real, file-backed event sink for an MCP mutation's CONFIRM/apply path,
/// mirroring `main.rs::open_event_sink`'s real-apply branch exactly:
///
/// - resolves the events dir from `telemetry.location` if configured, else
///   `cache::events_dir_for(vault_root)`;
/// - prunes by retention and enforces the size cap before opening (same order as
///   the CLI);
/// - opens the daily file via [`EventSink::open`], falling back to
///   [`EventSink::discard`] if the dir can't be resolved or opened.
///
/// **Best-effort by contract:** every failure mode degrades to an in-memory
/// `discard` sink rather than erroring, so telemetry can never block, fail, or
/// roll back the mutation. The returned sink still mints a real trace id (used as
/// the report's `trace_id`) even in the degraded case.
///
/// Use this ONLY on the confirm/apply path. Dry-run must keep `EventSink::discard`
/// directly (the CLI's dry-run branch does the same) so a plan-only call persists
/// nothing.
pub(crate) fn open_mutation_event_sink(ctx: &VaultContext) -> EventSink {
    let ids = IdGen::new();
    let clock = Clock::System;
    let start_ts = clock.now_rfc3339();

    let config = ctx.config();
    let telemetry = config.vault_config.telemetry.as_ref();
    let dir = telemetry
        .and_then(|t| t.location.clone())
        .map(camino::Utf8PathBuf::from)
        .or_else(|| {
            crate::cache::events_dir_for(&ctx.vault_root)
                .ok()
                .map(|(_, d)| d)
        });
    let retention = telemetry
        .and_then(|t| t.retention)
        .unwrap_or(crate::standards::DEFAULT_RETENTION);

    if let Some(dir) = dir.as_ref() {
        let today = &start_ts[..10];
        crate::telemetry::store::prune_events(dir, retention, today);
        crate::telemetry::store::enforce_size_cap(
            dir,
            crate::telemetry::store::EVENTS_SIZE_CAP_BYTES,
            today,
        );
        EventSink::open(dir, start_ts, ids, clock)
            .unwrap_or_else(|_| EventSink::discard(IdGen::new(), Clock::System))
    } else {
        EventSink::discard(ids, clock)
    }
}
