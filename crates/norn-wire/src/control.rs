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
//! reports the vault's [`ServingState`] (`cold | opening | ready`). It carries
//! NO writer-progress counter: liveness is proven by frames arriving, not by a
//! counter a poller inspects, so no reader for one exists. Per the 2026-07-17
//! amendment there is no Direct fallback — a client that gets no pong summons an
//! owner; an owner that goes silent past the client's stall budget is an
//! owner-health event, never a reroute.
//!
//! # The framed request protocol
//!
//! **One request is a stream of zero-or-more [`OwnerFrame::Progress`] frames
//! followed by exactly one terminal frame** ([`OwnerFrame::is_terminal`] names
//! the split). The owner emits a progress frame at least every
//! [`PROGRESS_HEARTBEAT`] while a request's work is in flight; the client treats
//! any frame as proof of life and only INTER-FRAME silence past its stall budget
//! as a hung owner. That is what lets a mutation run longer than the budget
//! without its own client abandoning it into ADR 0011's no-safe-retry
//! uncertainty, while a genuinely wedged owner still gets the stall verdict.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{
    ApplyParams, ApplyReport, AuditParams, AuditReport, CountParams, CountReport, DeleteParams,
    DescribeParams, DescribeReport, EditParams, EditReport, FindParams, FindReport, GetParams,
    GetReport, MoveParams, NewParams, NewReport, RepairParams, RepairReport, RewriteWikilinkParams,
    SetParams, SetReport, ValidateParams, ValidateReport,
};

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

/// How often the owner emits an [`OwnerFrame::Progress`] frame while a request's
/// work is in flight — the heartbeat FLOOR, not a schedule: milestone-driven
/// frames may arrive sooner.
///
/// It is named here, beside the frame it paces, because both sides depend on the
/// same relation: the client's stall budget must be a comfortable multiple of
/// this, so ordinary scheduling jitter on a busy owner can never be mistaken for
/// silence. `norn-wire` holds no logic — this is a shared constant of the
/// protocol, exactly like [`CONTROL_PROTOCOL`].
pub const PROGRESS_HEARTBEAT: Duration = Duration::from_secs(1);

/// What kind of work an owner is reporting progress for.
///
/// A typed tag, not prose: a client decides what to render (or whether to render
/// at all) from the variant, never by matching on a message (invariant 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProgressPhase {
    /// The one-shot warm-up build is in flight; the request is queued behind it.
    /// This is the phase a request lands in on a `cold`/`opening` owner.
    Warming,
    /// A read is running against the warm cache.
    Reading,
    /// A mutation is running under the owner's single-writer lock.
    Applying,
}

/// One in-flight progress observation: the [`ProgressPhase`] plus optional
/// milestone units.
///
/// `done` / `total` are OPTIONAL because a milestone is reported only where the
/// owner can count one without doing extra work — progress must never cost more
/// than the work it describes. Absent units still carry the phase, which is the
/// load-bearing part: a frame arrived, so the owner is alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub phase: ProgressPhase,
    /// Units completed so far, when the owner counts them cheaply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done: Option<u64>,
    /// Units expected in total, when known up front.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

impl Progress {
    /// A unit-less observation: the phase alone.
    pub fn new(phase: ProgressPhase) -> Self {
        Self {
            phase,
            done: None,
            total: None,
        }
    }

    /// Attach the completed-units milestone.
    pub fn with_done(mut self, done: u64) -> Self {
        self.done = Some(done);
        self
    }

    /// Attach the expected-total milestone.
    pub fn with_total(mut self, total: Option<u64>) -> Self {
        self.total = total;
        self
    }
}

/// Client -> owner. One JSON object per line.
///
/// `PartialEq` but not `Eq`: the `Apply` variant carries an [`ApplyParams`] whose
/// typed `plan` ([`MigrationPlan`](crate::MigrationPlan)) embeds a
/// `serde_json::Value` op-`fields` payload (not `Eq`), exactly as [`OwnerFrame`]'s
/// cascade-report variants (`ApplyReport`) are.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// A `get` request: resolve the targets and return their full facet sets.
    Get { params: GetParams },
    /// A `describe` request: the vault structure, plus a contents-summary with
    /// `--data`.
    Describe { params: DescribeParams },
    /// A `validate` request: run the standards engine over the warm graph and
    /// return the (triage-filtered) findings. Read-only.
    Validate { params: ValidateParams },
    /// A `repair` request: run the engine, filter findings, and build a
    /// deterministic `MigrationPlan` — WITHOUT applying it. Read-only (`apply`
    /// executes the plan).
    Repair { params: RepairParams },
    /// An `audit` request: read the per-vault mutation event stream (the durable
    /// JSONL store), filter, and return the newest-first matches. Read-only —
    /// the OWNER reads the store (it is co-located with the vault's state home;
    /// an off-filesystem client could not), like `get --format markdown`.
    Audit { params: AuditParams },
    /// A `set` request: mutate a document's frontmatter fields. Applies when
    /// `confirm` is set, else forecasts. The owner serializes writes under its
    /// single-writer lock.
    Set { params: SetParams },
    /// A `new` request: create a document from a rule template / explicit path /
    /// inbox. Applies when `confirm` is set, else forecasts.
    New { params: NewParams },
    /// An `edit` request: apply atomic content-anchored body edits to one
    /// document. Applies when `confirm` is set, else forecasts.
    Edit { params: EditParams },
    /// A `move` request: relocate a document (or folder) and cascade-rewrite
    /// backlinks. Applies when `confirm` is set, else forecasts.
    Move { params: MoveParams },
    /// A `delete` request: remove a document, optionally redirecting its incoming
    /// links. Applies when `confirm` is set, else forecasts.
    Delete { params: DeleteParams },
    /// A `rewrite-wikilink` request: rewrite `[[old]]` → `[[new]]` vault-wide.
    /// Applies when `confirm` is set, else forecasts.
    RewriteWikilink { params: RewriteWikilinkParams },
    /// An `apply` request: execute an already-parsed, schema-checked
    /// `MigrationPlan` (carried typed in `params.plan`). Applies when `confirm`
    /// is set, else forecasts.
    Apply { params: ApplyParams },
}

/// Owner -> client. One JSON object per line.
///
/// One request yields zero-or-more [`Progress`](OwnerFrame::Progress) frames and
/// then exactly one TERMINAL frame — every other variant here. See
/// [`is_terminal`](OwnerFrame::is_terminal) and the module docs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum OwnerFrame {
    /// A non-terminal in-flight observation: the request is still running and
    /// the owner is alive. Zero or more of these precede the terminal frame; a
    /// client that renders nothing still consumes them, because consuming one is
    /// what resets its silence budget.
    Progress { progress: Progress },
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
    },
    /// The answer to `Probe`: the vault's live document count.
    Probe { document_count: u64 },
    /// The answer to `Find`: the matched, projected, paged document set.
    Find { report: FindReport },
    /// The answer to `Count`: the total, distribution, or nested group tree.
    Count { report: CountReport },
    /// The answer to `Get`: the resolved records, notes, and (for markdown) the
    /// single doc's exact source.
    Get { report: GetReport },
    /// The answer to `Describe`: the vault structure and optional data summary.
    Describe { report: DescribeReport },
    /// The answer to `Validate`: the findings, summary body, and run counts.
    Validate { report: ValidateReport },
    /// The answer to `Repair`: the deterministic `MigrationPlan` (as its
    /// pretty-JSON string) plus the bare-summary finding tally and exit signal.
    Repair { report: RepairReport },
    /// The answer to `Audit`: the newest-first matched events from the mutation
    /// stream (empty when the stream is absent or nothing matched).
    Audit { report: AuditReport },
    /// The answer to `Set`: the frontmatter change report (applied or forecast,
    /// or a coded `outcome = refused` on a clean pre-write decline).
    Set { report: SetReport },
    /// The answer to `New`: the creation report (applied or forecast, or a coded
    /// `outcome = refused`).
    New { report: NewReport },
    /// The answer to `Edit`: the body-edit report (applied or forecast, or a
    /// coded `outcome = refused` on a clean pre-write decline).
    Edit { report: EditReport },
    /// The answer to `Move` / `Delete` / `RewriteWikilink`: the shared typed
    /// [`ApplyReport`] (which now lives in this crate — see `crate::mutate`). A
    /// refusal rides in `outcome = refused` + `operations[].error`, so it stays
    /// exit 2 without a distinct frame. Applied/forecast reports carry the cascade.
    Move { report: ApplyReport },
    /// The answer to `Delete` — the shared typed [`ApplyReport`].
    Delete { report: ApplyReport },
    /// The answer to `RewriteWikilink` — the shared typed [`ApplyReport`].
    RewriteWikilink { report: ApplyReport },
    /// The answer to `Apply` — the shared typed [`ApplyReport`] for the executed
    /// plan. A refusal (an owner-set precondition mismatch, a containment
    /// violation) rides in `outcome = refused` + `operations[]`/`preconditions[]`,
    /// so it stays exit 2 without a distinct frame.
    Apply { report: ApplyReport },
    /// A well-formed request the owner could not carry out for a
    /// non-cache reason — a bad predicate, an unresolvable `--links-to`
    /// target. Distinct from [`Error`](OwnerFrame::Error): the owner stays
    /// alive (no exit-to-heal) and the client surfaces this as an operational
    /// failure, not an owner-health event.
    ///
    /// `hints` carries the soft-landing lines structured alongside the headline
    /// (NRN-361): the CLI renders them as `hint:` lines, an MCP client reads
    /// them as fields. Absent on the wire when empty (the common case today —
    /// the owner does not yet compute rejection hints), so this is backward
    /// compatible with a `{ message }`-only frame.
    Rejected {
        message: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        hints: Vec<String>,
    },
    /// A fatal owner-side error (e.g. a `CacheError` — the db is disposable
    /// derivation, so any cache error is exit-to-heal). The owner is
    /// terminating; the client surfaces this as an owner-health error and a
    /// resummon rebuilds. Never a Direct fallback (ADR 0017).
    Error { message: String },
}

impl OwnerFrame {
    /// Whether this frame ENDS its request. Exactly one terminal frame closes
    /// every request, and this predicate is what the client's frame loop
    /// returns on (`norn-client`'s `OwnerSession::request`): a frame it calls
    /// non-terminal is consumed as proof of life and the loop reads again.
    ///
    /// The match is EXHAUSTIVE by variant, never a negated `matches!`: the next
    /// frame kind (a step-aside or shutdown notice) must state which side of the
    /// split it belongs on rather than inheriting "terminal" by default and
    /// silently ending a request that is still running.
    pub fn is_terminal(&self) -> bool {
        match self {
            OwnerFrame::Progress { .. } => false,
            OwnerFrame::Pong { .. }
            | OwnerFrame::Probe { .. }
            | OwnerFrame::Find { .. }
            | OwnerFrame::Count { .. }
            | OwnerFrame::Get { .. }
            | OwnerFrame::Describe { .. }
            | OwnerFrame::Validate { .. }
            | OwnerFrame::Repair { .. }
            | OwnerFrame::Audit { .. }
            | OwnerFrame::Set { .. }
            | OwnerFrame::New { .. }
            | OwnerFrame::Edit { .. }
            | OwnerFrame::Move { .. }
            | OwnerFrame::Delete { .. }
            | OwnerFrame::RewriteWikilink { .. }
            | OwnerFrame::Apply { .. }
            | OwnerFrame::Rejected { .. }
            | OwnerFrame::Error { .. } => true,
        }
    }
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

    /// NRN-512: a unit-less progress frame is the phase alone — absent
    /// milestones stay off the wire and round-trip back as absent.
    #[test]
    fn progress_omits_absent_milestones_and_round_trips() {
        let frame = OwnerFrame::Progress {
            progress: Progress::new(ProgressPhase::Warming),
        };
        let line = serde_json::to_string(&frame).unwrap();
        assert_eq!(line, r#"{"op":"progress","progress":{"phase":"warming"}}"#);
        assert_eq!(serde_json::from_str::<OwnerFrame>(&line).unwrap(), frame);

        let with_units = OwnerFrame::Progress {
            progress: Progress::new(ProgressPhase::Applying)
                .with_done(3)
                .with_total(Some(12)),
        };
        let line = serde_json::to_string(&with_units).unwrap();
        assert_eq!(
            line,
            r#"{"op":"progress","progress":{"phase":"applying","done":3,"total":12}}"#
        );
        assert_eq!(
            serde_json::from_str::<OwnerFrame>(&line).unwrap(),
            with_units
        );
    }

    /// The protocol's load-bearing split (NRN-512): `Progress` is the ONLY
    /// non-terminal frame, so a client loop that returns on `is_terminal`
    /// returns on every answer shape — including a `Rejected` or an `Error`.
    #[test]
    fn progress_is_the_only_non_terminal_frame() {
        assert!(!OwnerFrame::Progress {
            progress: Progress::new(ProgressPhase::Reading),
        }
        .is_terminal());

        for terminal in [
            OwnerFrame::Probe { document_count: 0 },
            OwnerFrame::Rejected {
                message: "nope".into(),
                hints: Vec::new(),
            },
            OwnerFrame::Error {
                message: "boom".into(),
            },
        ] {
            assert!(terminal.is_terminal(), "{terminal:?} must end its request");
        }
    }

    #[test]
    fn progress_phase_is_lowercase_on_the_wire() {
        assert_eq!(
            serde_json::to_string(&ProgressPhase::Applying).unwrap(),
            r#""applying""#
        );
    }

    /// The heartbeat floor must stay a comfortable multiple below the client's
    /// stall budget, or ordinary jitter reads as silence. The budget itself
    /// lives in `norn-client`; this pins the wire half of the relation so a
    /// future edit to the heartbeat cannot quietly close the gap.
    #[test]
    fn the_heartbeat_floor_leaves_room_under_a_multi_second_budget() {
        assert!(
            PROGRESS_HEARTBEAT <= std::time::Duration::from_secs(2),
            "a heartbeat this slow leaves no jitter margin under a 5s budget"
        );
    }
}
