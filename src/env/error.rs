//! Warm-context error classification and corruption eviction.

use super::*;

/// A typed error the warm daemon can downcast to decide whether to evict the
/// whole `VaultContext`. Kept intentionally small and `anyhow`-downcastable.
#[derive(Debug, thiserror::Error)]
pub(crate) enum WarmContextError {
    /// The vault root can no longer be canonicalized (deleted, unmounted,
    /// permission-denied). The daemon should evict the context for this vault.
    #[error("vault root {root} is no longer accessible")]
    RootGone {
        root: Utf8PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Does this error chain carry a SQLite corruption-class failure
/// (`DatabaseCorrupt` / `NotADatabase`)? Drives warm mode's error-triggered
/// eviction (FIX-3). The rusqlite error is often wrapped (e.g. in `CacheError`),
/// so we walk the whole `anyhow` chain and downcast each cause.
pub(in crate::env) fn is_sqlite_corruption(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<rusqlite::Error>()
            .and_then(|e| e.sqlite_error_code())
            .is_some_and(|code| {
                matches!(
                    code,
                    rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
                )
            })
    })
}

impl VaultContext {
    /// Corruption-eviction seam (FIX-3): inspect a failed tool's error chain and,
    /// in warm mode, invalidate the current generation when the failure is a
    /// SQLite corruption-class error (`DatabaseCorrupt` / `NotADatabase`). The
    /// next request then reopens through the single-flight
    /// [`ensure_current`](Self::ensure_current) path (integrity_check → detect →
    /// rebuild) — the same self-heal a one-shot CLI gets for free. No-op in cold
    /// mode (each call already opens + verifies a fresh cache).
    ///
    /// Invalidation bumps the slot's monotonic floor above the current
    /// generation's number rather than nulling the pointer in place, so eviction
    /// and reopen both flow through the one open path and any request still
    /// draining on the corrupt generation is undisturbed (it drops with its last
    /// `Arc`).
    ///
    /// Trust framing (ADR 0005): warm mode verifies integrity once and never
    /// re-runs integrity_check by design. That holds because corruption
    /// *surfaces as errors*, and this error-evict-reverify loop re-establishes
    /// trust on the next request. Silent wrong-data corruption that raises no
    /// error is outside SQLite's own detection model too, so it is not in scope.
    ///
    /// Invalidation keys off the SCOPE's bound generation — the generation THIS
    /// request bound in `query_cache_warm` — NOT `slot.current` (NRN-253). Before
    /// `call_lock` retired the two coincided, but a corruption
    /// error on generation N must never invalidate a healthy generation N+1 that
    /// has since become current (the concurrent read pool makes that observable):
    /// mirrors how `WarmGuard::Drop` bumps the floor off its OWN generation's
    /// number rather than off whatever is current at drop time.
    ///
    /// The bound generation is the right key for TOOL-BODY errors only — the tool
    /// body reads through the connection the scope bound. An error from work that
    /// ran on a DIFFERENT generation (the post-apply increment commit binds
    /// `slot.current`, which can be newer than the scope's binding) must instead
    /// evict the generation it actually ran on, via the shared
    /// [`evict_generation_on_corruption`](Self::evict_generation_on_corruption)
    /// this method delegates to.
    pub(crate) fn note_tool_error(&self, scope: &RequestScope, err: &anyhow::Error) {
        self.evict_generation_on_corruption(scope.bound_generation(), err);
    }

    /// The ONE corruption-classification + eviction seam: when `err`'s chain
    /// carries a SQLite corruption-class failure (`DatabaseCorrupt` /
    /// `NotADatabase`), bump the warm slot's invalidation floor past generation
    /// `number`, so the next request reopens (integrity_check → detect → rebuild)
    /// through the single-flight `ensure_current` path. `number == 0` ("no
    /// generation") and cold mode are no-ops, as is a non-corruption error.
    ///
    /// Callers differ ONLY in which generation they key the eviction off:
    /// [`note_tool_error`](Self::note_tool_error) passes the scope's bound
    /// generation (tool-body attribution), while the post-apply increment path
    /// passes the generation the increment actually ran on. Sharing the
    /// classification here keeps the two from diverging while keeping their keys
    /// distinct.
    pub(in crate::env) fn evict_generation_on_corruption(&self, number: u64, err: &anyhow::Error) {
        let Mode::Warm(slot) = &self.mode else {
            return;
        };
        if number != 0 && is_sqlite_corruption(err) {
            slot.shared.floor.fetch_max(number + 1, Ordering::AcqRel);
        }
    }
}
