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

/// Extract a structured refusal envelope (NRN-220) from a single-op mutation
/// error, IF it is a recognized precondition/refusal carrying a stable machine
/// `code`.
///
/// Returns `Some` for the coded refusal types the single-op mutators
/// (`set`/`edit`/`new`) can raise — the rich apply-time
/// [`ApplyError`](crate::standards::apply::ApplyError) (CAS / precondition), a
/// [`ContainmentError`](crate::standards::apply::ContainmentError), the `edit`
/// anchor/CAS family ([`EditError`](crate::edit::transform::EditError)), and
/// `vault.new`'s [`PreflightError`](crate::new::validate::PreflightError). Returns
/// `None` for everything else — plain-prose validation errors, IO, internal
/// failures — so those still propagate as a bare MCP `Err` rather than being
/// laundered into a misleading `internal-error` structured refusal.
///
/// This is the deliberate counterpart to
/// [`ApplyError::from_anyhow`](crate::apply_report::ApplyError::from_anyhow),
/// which ALWAYS produces an envelope (falling back to `internal-error`): here a
/// non-refusal must stay a non-refusal. `set`'s schema-validation prose is
/// intentionally in the `None` bucket until it is coded (NRN-221).
pub(crate) fn refusal_from_error(e: &anyhow::Error) -> Option<crate::apply_report::ApplyError> {
    use crate::apply_report::ApplyError as Envelope;
    use crate::standards::apply::{ApplyError as RichApplyError, ContainmentError};

    if let Some(rich) = e.downcast_ref::<RichApplyError>() {
        return Some(Envelope::from_rich(rich));
    }
    if let Some(c) = e.downcast_ref::<ContainmentError>() {
        return Some(Envelope::from_containment(c));
    }
    if let Some(ed) = e.downcast_ref::<crate::edit::transform::EditError>() {
        return Some(Envelope {
            code: ed.code().to_string(),
            message: ed.to_string(),
            path: ed.path().map(str::to_string),
        });
    }
    if let Some(pf) = e.downcast_ref::<crate::new::validate::PreflightError>() {
        return Some(Envelope {
            code: pf.code().to_string(),
            message: pf.to_string(),
            path: pf.path(),
        });
    }
    None
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

#[cfg(test)]
mod refusal_tests {
    use super::refusal_from_error;

    /// A rich apply-time `ApplyError` (the TOCTOU CAS path) is recognized and
    /// keeps its stable kebab code — the machine-branchable value survives.
    #[test]
    fn rich_apply_error_yields_its_code() {
        let e = anyhow::anyhow!(crate::standards::apply::ApplyError::StaleDocumentHash {
            path: "a.md".into(),
            expected: "aaaa".into(),
            actual: "bbbb".into(),
        });
        let env = refusal_from_error(&e).expect("a rich ApplyError is a recognized refusal");
        assert_eq!(env.code, "stale-document-hash");
        assert_eq!(env.path.as_deref(), Some("a.md"));
    }

    /// The `edit` anchor family (`EditError`) is recognized with its code.
    #[test]
    fn edit_error_yields_its_code() {
        let e: anyhow::Error = crate::edit::transform::EditError::StrNotFound {
            index: 0,
            kind: "str_replace",
            anchor: "nope".into(),
        }
        .into();
        assert_eq!(
            refusal_from_error(&e).expect("recognized").code,
            "anchor-not-found"
        );
    }

    /// `vault.new`'s `PreflightError` is recognized with its code + path.
    #[test]
    fn preflight_error_yields_its_code() {
        let e: anyhow::Error =
            crate::new::validate::PreflightError::DestinationExists("x.md".into()).into();
        let env = refusal_from_error(&e).expect("recognized");
        assert_eq!(env.code, "destination-exists");
        assert_eq!(env.path.as_deref(), Some("x.md"));
    }

    /// THE load-bearing invariant (NRN-220/221): an UNRECOGNIZED error — e.g.
    /// `set`'s still-uncoded schema-validation prose — returns `None`, so it stays
    /// a bare MCP `Err` and is NOT laundered into a misleading `internal-error`
    /// structured refusal. This is what keeps the deferred `set` scope honest.
    #[test]
    fn unrecognized_prose_error_is_not_a_refusal() {
        let e = anyhow::anyhow!("field 'status' is not one of the allowed values");
        assert!(
            refusal_from_error(&e).is_none(),
            "uncoded prose must propagate as a bare Err, not a laundered refusal"
        );
    }
}
