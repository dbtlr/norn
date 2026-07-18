//! The surface-neutral command-core seam: `trait Request` + one generic
//! [`dispatch`] (ADR 0016, phase 1 — NRN-291).
//!
//! A routable command flows through three beats, and this module owns the
//! middle one:
//!
//! 1. **Adapter** (surface-specific, in `lib.rs` / the MCP `#[tool]` wrapper).
//!    Parse the surface's own input into the canonical request vocabulary — the
//!    params struct that implements [`Request`] — and, on the CLI side, render
//!    the returned report. Everything that is CLI-only lives HERE: the
//!    `--plan` routing gate, `--format` / `--out` rendering knobs, and — for the
//!    mutation verbs migrated in later phases — the confirm/TTY ladder and stdin
//!    gates. Those pre-dispatch guards stay adapter-side and are intentionally
//!    NOT part of the trait; a knob with no [`Request`] field never reaches the
//!    wire, which is exactly why the exclusion is enforced by construction.
//! 2. **Dispatch** (this module, surface-neutral). Either serialize the request
//!    to a live warm `norn serve` daemon over the control socket — which runs
//!    the SAME [`Request::execute`] body against its warm cache and returns a
//!    `structuredContent` envelope the client rebuilds via
//!    [`Request::reconstruct`] — or, when no daemon is live (or a forced-direct
//!    flag applies), execute locally against a cold [`VaultEnv`]. Both branches
//!    produce the identical [`Request::Report`], so routed and direct output are
//!    byte-for-byte equal (ADR 0005). The socket mechanics (probe, handshake,
//!    build-skew gate, pre-send/post-send fallback) are REUSED from the existing
//!    routing skeleton (`crate::route_read`); this module only generalizes the
//!    entry over the trait.
//! 3. **Execute** (surface-neutral, [`Request::execute`]). The ONE
//!    implementation of the command against a `&VaultEnv` + `&RequestScope`.
//!    The MCP `#[tool]` handler body IS this function, and the daemon and the
//!    cold CLI path both call it — there is no second copy.
//!
//! ## Routing policy
//!
//! Phase 1 migrates only `repair --plan`, a READ. Reads are idempotent, so
//! dispatch runs them through [`route_read`](crate::route_read) with the
//! read-safe `FallbackAfterSend::Fallback` posture: any failure, pre- or
//! post-send, silently falls back to a verified local run. [`Request::on_tool_error`]
//! defaults to the read posture ([`OnToolError::FallBackDirect`]). When the
//! mutation verbs migrate (NRN-292+) they add a send-commit policy knob here
//! (a `Commit` posture for a real apply, mirroring `route_call`) alongside the
//! adapter-side confirm ladder — the trait grows one method, not a fork.
//!
//! ## Cache-maintenance contract (the `routed` bit)
//!
//! A ROUTED command performs NO local cache-maintenance side effects — the
//! daemon owns its warm cache, so it (not this process) is responsible for GC
//! eviction and the throttled prune sweep. The CLI adapter must therefore skip
//! the tail GC trigger `maybe_spawn_sweep` (`lib.rs`, the shared `run` tail,
//! gated by `tail_sweep_fires`) whenever a request
//! routed, exactly reproducing the pre-NRN-291 early-return: on main a routed
//! read `return`ed at the call site and never reached the sweep, while a DIRECT
//! (cold local) run fell through to it. [`dispatch`] surfaces which happened via
//! [`Dispatched::routed`] so the adapter can gate the sweep — and every verb
//! migrated in NRN-292+ inherits this parity by threading the same bit into the
//! same gate, rather than each re-deriving it.

use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};

use crate::env::{RequestScope, VaultEnv};
use crate::service::OnToolError;

/// A surface-neutral request: the canonical params vocabulary a warm daemon can
/// serve or a cold [`VaultEnv`] can execute locally, producing a typed report.
///
/// Implemented by the params struct (e.g. `RepairParams`). `Serialize` carries
/// the request over the daemon socket; the report crosses back as the tool's
/// `structuredContent`, rebuilt by [`reconstruct`](Request::reconstruct).
pub(crate) trait Request: serde::Serialize {
    /// The MCP tool name this request dispatches to (e.g. `"vault.repair"`).
    /// MUST equal the `#[tool(name = ...)]` literal and the `tool_names` const;
    /// the catalog parity test pins all three (NRN-291).
    const TOOL: &'static str;

    /// The typed report both `execute` and `reconstruct` produce. The CLI
    /// adapter renders it; the wire schema is frozen serde of this type.
    type Report;

    /// The ONE implementation: run the command against a `&VaultEnv` +
    /// `&RequestScope`. The MCP `#[tool]` handler body is exactly this; the
    /// daemon and the cold CLI path both call it.
    fn execute(&self, env: &VaultEnv, scope: &RequestScope) -> Result<Self::Report>;

    /// Rebuild the report from a daemon `structuredContent` envelope. For a
    /// report whose wire value is its own serialization this is plain serde;
    /// a failure here falls back to a verified local run.
    fn reconstruct(structured: &serde_json::Value) -> Result<Self::Report>;

    /// What an `isError: true` tool result means for this request. Defaults to
    /// the read-safe posture: fall back to a direct run rather than render a
    /// daemon-side error payload as a real result.
    fn on_tool_error() -> OnToolError {
        OnToolError::FallBackDirect
    }
}

/// The result of a [`dispatch`] call: the rendered `outcome` paired with whether
/// a live warm daemon `routed` it.
///
/// `routed` is the dispatch seam's cache-maintenance contract (see the module
/// doc): `true` means the daemon served the request and owns its cache upkeep,
/// so the CLI adapter MUST skip the tail GC trigger `maybe_spawn_sweep`; `false` means a cold local
/// run, which still sweeps. It is carried alongside `outcome` (rather than folded
/// into the `Ok`) precisely so a routed FAILURE — a daemon-side refusal, an
/// unreadable envelope committed post-send — also suppresses the sweep, matching
/// main's early-return, which skipped the sweep on any routed result, error or
/// not.
pub(crate) struct Dispatched {
    /// Whether a live warm daemon served the request (routed) vs. a cold local
    /// execution (direct). Gates the CLI adapter's tail GC trigger
    /// `maybe_spawn_sweep` (via `tail_sweep_fires`).
    pub(crate) routed: bool,
    /// The rendered exit code, or the error the top level maps to one.
    pub(crate) outcome: Result<i32>,
}

/// Dispatch `req`: serve it from a live warm daemon when one answers this vault
/// (and no forced-direct flag applies), otherwise execute it locally against a
/// cold [`VaultEnv`]. Either way the resulting [`Request::Report`] is rendered
/// by `render` — the surface's one report renderer, shared by both branches so
/// routed and direct output cannot drift — and the [`Dispatched`] return carries
/// the `routed` bit the caller gates its tail cache-maintenance on.
///
/// `render` is `Fn` (not `FnOnce`) because exactly one branch runs it at
/// runtime but both reference it; a routed run renders inside the skeleton
/// (after any daemon-side operator notes are re-emitted, preserving stderr
/// ordering), a local run renders after draining the request scope's notes.
pub(crate) fn dispatch<R: Request>(
    req: &R,
    cwd: &Utf8Path,
    config_path: Option<&Utf8PathBuf>,
    no_cache_refresh: bool,
    explicit_config: bool,
    verbose: bool,
    render: impl Fn(R::Report) -> Result<i32>,
) -> Dispatched {
    // Beat 2 — DISPATCH. A live warm daemon serves the request over the socket,
    // byte-identically to local execution. Reuses the read-routing skeleton
    // (probe, handshake, build-skew gate, operator-note re-emission,
    // pre-send/post-send fallback). Unix-only: the daemon rides a Unix-domain
    // socket, so on other targets every request executes locally (the ONE
    // `#[cfg]` seam — future migrated verbs inherit it, no per-verb stub).
    #[cfg(unix)]
    {
        if !crate::routing_forced_direct(explicit_config, no_cache_refresh) {
            let arguments = match serde_json::to_value(req) {
                Ok(arguments) => arguments,
                // A request that cannot even serialize never routed — report the
                // error as a DIRECT outcome so the tail sweep still runs, exactly
                // as the pre-NRN-291 direct path would have on a local error.
                Err(error) => {
                    return Dispatched {
                        routed: false,
                        outcome: Err(error.into()),
                    };
                }
            };
            let spec = crate::CallSpec {
                tool: R::TOOL,
                arguments,
                on_tool_error: R::on_tool_error(),
                verbose,
            };
            // `route_read` renders via the `emit` closure on success and returns
            // `None` on any read-safe miss/failure (no daemon, forced pre-send
            // fall-back, unreadable envelope) — falling through to local below.
            // A `Some` (Ok OR Err) means the request COMMITTED to routing: the
            // daemon owns cache upkeep, so mark it routed and let the caller skip
            // the tail sweep, mirroring main's early-return semantics exactly.
            if let Some(outcome) = crate::route_read(cwd, spec, R::reconstruct, &render) {
                return Dispatched {
                    routed: true,
                    outcome,
                };
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (explicit_config, verbose);
    }

    // Beat 3 — EXECUTE (DIRECT). No daemon (or a forced-direct flag): run the ONE
    // implementation locally against a cold VaultEnv. `open_cold` honors
    // `--no-cache-refresh` so the local path reproduces the pre-NRN-291 direct
    // path's cache-refresh behavior, and `--config` via `config_path`. The result
    // is captured as `outcome` (NOT `?`-propagated out of `dispatch`) so a cold
    // failure still returns `routed: false` — the direct path swept on error too.
    let outcome = (|| -> Result<i32> {
        let env = VaultEnv::open_cold(cwd, config_path, no_cache_refresh)?;
        let scope = env.begin_request()?;
        let report = req.execute(&env, &scope);
        // Cold-path operator notes (fidelity): drain the request scope to stderr,
        // matching the direct path's own note plumbing. The cold cache open emits
        // its lock-contention note via `eprintln!` inline during `execute`, so
        // this drain is normally empty for repair; it stays here so any
        // scope-buffered note lands on stderr before the rendered output, exactly
        // like the routed path re-emits daemon-side notes before rendering.
        for note in scope.take_operator_notes() {
            eprintln!("{note}");
        }
        render(report?)
    })();
    Dispatched {
        routed: false,
        outcome,
    }
}
