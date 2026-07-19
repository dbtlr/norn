//! The owner control plane: liveness ping/pong and the trivial routed request
//! the summoner exercises before the read verbs land (ADR 0013 / 0017).
//!
//! Pure serde types — the frames the client sends and the owner answers. The
//! transport is one JSON object per line over the per-vault Unix socket; the
//! owner serves exactly one vault (the socket is keyed by vault-root hash +
//! build fingerprint), so a ping needs no vault selector — the socket already
//! scopes it. `norn-wire` never opens a socket or a cache; it only names the
//! shapes both sides encode.
//!
//! ADR 0013's control-plane contract carries forward inside the owner: a `Pong`
//! reports the vault's [`ServingState`] (`cold | opening | ready`) plus its
//! [`WriterProgress`] `{ busy, sequence }`. Per the 2026-07-17 amendment there
//! is no Direct fallback — a client that gets no pong summons an owner; a busy
//! writer whose sequence has stalled past the owner's stall budget is an
//! owner-health event, never a reroute.

use serde::{Deserialize, Serialize};

use crate::{CountParams, CountReport, FindParams, FindReport};

/// The control-frame protocol version. Under ADR 0012's amendment the socket is
/// keyed by build fingerprint, so a client can never reach a mismatched owner;
/// this constant is the demoted sanity assert both sides still check.
pub const CONTROL_PROTOCOL: u32 = 1;

/// Whether the owner's single vault has no warm context yet, is warming, or is
/// ready to serve reads (ADR 0013). A summon that connects mid-warm-up sees
/// `Opening` and waits for `Ready` rather than falling back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServingState {
    /// No warm context yet (the owner just bound its socket; warm-up pending).
    Cold,
    /// The one-shot full build (warm-up) is in flight.
    Opening,
    /// The warm context is built and serving reads.
    Ready,
}

/// Opaque per-vault writer progress (ADR 0013). `sequence` is forward progress,
/// not wall-clock: it advances on open transitions, completed liveness work,
/// bulk chunk boundaries, and terminal completion. A live idle writer is
/// healthy; only `busy` with `sequence` unchanged past the stall budget is hung.
///
/// The wire twin of `norn_core::cache::WriterProgress` — the owner maps its
/// engine-side value onto this so the client (which never links `norn-core`)
/// can read it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriterProgress {
    pub busy: bool,
    pub sequence: u64,
}

/// Client -> owner. One JSON object per line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ClientFrame {
    /// Liveness + serving-state probe. O(1) on the owner — it touches no vault
    /// filesystem, just reports the serving state and a progress snapshot.
    Ping { protocol: u32 },
    /// The trivial routed read exercised end-to-end before the read verbs land
    /// (NRN-345): count the vault's documents through the owner's warm
    /// `serve_read`. Retained alongside the real read verbs as a liveness probe.
    Probe,
    /// A `find` request: run the filter/sort/paging query through the warm cache.
    Find { params: FindParams },
    /// A `count` request: run the filter query and group by `--by`.
    Count { params: CountParams },
}

/// Owner -> client. One JSON object per line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum OwnerFrame {
    /// Proof of life plus the vault's serving/progress state (answer to `Ping`).
    Pong {
        protocol: u32,
        version: String,
        /// The owner's build fingerprint (short form). Present so a future
        /// resident/managed tier can sanity-assert it; the ephemeral client
        /// already trusts the build-keyed socket.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        build: Option<String>,
        pid: u32,
        serving: ServingState,
        writer_progress: WriterProgress,
    },
    /// The answer to `Probe`: the vault's live document count.
    Probe { document_count: u64 },
    /// The answer to `Find`: the matched, projected, paged document set.
    Find { report: FindReport },
    /// The answer to `Count`: the total, distribution, or nested group tree.
    Count { report: CountReport },
    /// A well-formed request the owner could not carry out for a
    /// non-cache reason — a bad predicate, an unresolvable `--links-to`
    /// target. Distinct from [`Error`](OwnerFrame::Error): the owner stays
    /// alive (no exit-to-heal) and the client surfaces this as an operational
    /// failure, not an owner-health event.
    Rejected { message: String },
    /// A fatal owner-side error (e.g. a `CacheError` — the db is disposable
    /// derivation, so any cache error is exit-to-heal). The owner is
    /// terminating; the client surfaces this as an owner-health error and a
    /// resummon rebuilds. Never a Direct fallback (ADR 0017).
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_roundtrips_as_one_line() {
        let frame = ClientFrame::Ping {
            protocol: CONTROL_PROTOCOL,
        };
        let line = serde_json::to_string(&frame).unwrap();
        assert!(!line.contains('\n'));
        assert_eq!(serde_json::from_str::<ClientFrame>(&line).unwrap(), frame);
    }

    #[test]
    fn probe_frame_has_no_fields() {
        assert_eq!(
            serde_json::to_string(&ClientFrame::Probe).unwrap(),
            r#"{"op":"probe"}"#
        );
    }

    #[test]
    fn pong_omits_absent_build() {
        let frame = OwnerFrame::Pong {
            protocol: CONTROL_PROTOCOL,
            version: "0.0.0".into(),
            build: None,
            pid: 42,
            serving: ServingState::Ready,
            writer_progress: WriterProgress {
                busy: false,
                sequence: 3,
            },
        };
        let line = serde_json::to_string(&frame).unwrap();
        assert!(
            !line.contains("build"),
            "absent build must not serialize: {line}"
        );
        assert_eq!(serde_json::from_str::<OwnerFrame>(&line).unwrap(), frame);
    }

    #[test]
    fn serving_state_is_lowercase_on_the_wire() {
        assert_eq!(
            serde_json::to_string(&ServingState::Opening).unwrap(),
            r#""opening""#
        );
    }
}
