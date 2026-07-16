//! Per-request scope threaded to each tool body (NRN-253).

use super::*;

/// Per-request scope (NRN-253): the state that must be private to ONE in-flight
/// request once the daemon no longer serializes tool bodies with `call_lock`.
///
/// Three things previously lived context-global (or slot-global) and were only
/// safe because `call_lock` guaranteed at most one request per vault ran at a
/// time; NRN-253 moves each into this per-request value, threaded explicitly to
/// the tool body alongside `&VaultContext`:
///
/// - **The bound config** (`config`). A request binds the current
///   `Arc<LoadedConfig>` at its boundary ([`VaultContext::begin_request`]) and
///   reads it via [`RequestScope::config`] for its whole life, so a concurrent
///   request's `begin_request` swapping the stored config cannot split-brain this
///   one between two reads (NRN-251's `(generation, config)` binding, now
///   structural).
/// - **The operator-note buffer** (`notes`). Notes a request produces (e.g. the
///   write-lock contention note) accumulate here and are drained by
///   `run_wrapped` into exactly this request's tool envelope — concurrent
///   requests can no longer interleave notes into each other's envelopes.
/// - **The bound generation** (`bound_generation`). Stamped by
///   [`query_cache_warm`](VaultContext::query_cache_warm) when the request binds
///   its generation, read by [`note_tool_error`](VaultContext::note_tool_error)
///   to key corruption invalidation off the generation THIS request used — not
///   whichever generation happens to be `current` when the error surfaces. Warm-
///   only; inert (stays 0) in cold mode. `0` means "no generation bound yet".
///
/// The scope's whole lifecycle — create → tool body → error attribution → note
/// drain — runs on the ONE `spawn_blocking` thread `run_wrapped` dispatches (it
/// never crosses an `.await`, and no other thread ever holds a reference to it),
/// so single-threaded interior mutability (`RefCell` / `Cell`) suffices. The
/// type stays `Send` — a scope may be MOVED to another thread whole — but
/// nothing requires `Sync`, and keeping it `!Sync` documents that a scope is
/// never SHARED across threads.
pub(crate) struct RequestScope {
    /// The `Arc<LoadedConfig>` bound at the request boundary; held for the whole
    /// request so config is stable across every read within it.
    config: Arc<LoadedConfig>,
    /// This request's operator notes, drained by `run_wrapped` into its envelope.
    notes: RefCell<Vec<String>>,
    /// The generation number this request bound in `query_cache_warm`
    /// (`0` = none bound). Read by `note_tool_error` for corruption attribution.
    bound_generation: Cell<u64>,
}

// `LoadedConfig` is not `Debug`, so derive is out; a manual impl lets a request
// scope appear in test `Result` messages (e.g. `begin_request().expect_err(..)`)
// without printing the whole resolved config.
impl std::fmt::Debug for RequestScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestScope")
            .field("notes", &self.notes)
            .field("bound_generation", &self.bound_generation)
            .finish_non_exhaustive()
    }
}

impl RequestScope {
    /// Build a scope bound to `config`, with an empty note buffer and no
    /// generation bound yet.
    pub(in crate::env) fn new(config: Arc<LoadedConfig>) -> Self {
        Self {
            config,
            notes: RefCell::new(Vec::new()),
            bound_generation: Cell::new(0),
        }
    }

    /// The config bound at this request's boundary. Tool bodies read config
    /// through here (not `VaultContext::config`) so it stays request-stable.
    pub(crate) fn config(&self) -> Arc<LoadedConfig> {
        Arc::clone(&self.config)
    }

    /// Record an operator note for this request. Drained by `run_wrapped` into
    /// the tool envelope's `operator_notes` (NRN-215).
    pub(crate) fn push_operator_note(&self, note: impl Into<String>) {
        self.notes.borrow_mut().push(note.into());
    }

    /// Drain and return the notes accumulated for this request. Called by
    /// `run_wrapped` immediately after the tool body, so the returned notes
    /// belong to exactly this request.
    pub(crate) fn take_operator_notes(&self) -> Vec<String> {
        self.notes.take()
    }

    /// Stamp the generation this request bound (warm mode). Written once, right
    /// after `ensure_current` returns in `query_cache_warm`.
    pub(in crate::env) fn bind_generation(&self, number: u64) {
        self.bound_generation.set(number);
    }

    /// The generation this request bound (`0` = none). Read by `note_tool_error`.
    pub(in crate::env) fn bound_generation(&self) -> u64 {
        self.bound_generation.get()
    }
}
