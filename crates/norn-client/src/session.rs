//! Connecting to a summoned owner and speaking the norn-wire control plane.
//!
//! Synchronous: a CLI invocation is one short-lived process. There is NO
//! in-process cache open anywhere here.
//!
//! # One frame loop, one silence budget
//!
//! A request is one write followed by [`request`](OwnerSession::request)'s frame
//! loop: every non-terminal frame ([`OwnerFrame::is_terminal`] names the split —
//! today that is [`OwnerFrame::Progress`], whose observation goes to the
//! session's [`ProgressSink`]) leaves the loop reading on; the single terminal
//! frame ends the request. [`STALL_BUDGET`] is the socket's PER-READ deadline
//! (`SO_RCVTIMEO`), so any byte restarts it. A long mutation that heartbeats
//! therefore never trips it, while a wedged owner that emits nothing surfaces as
//! [`ClientError::OwnerHealth`] at the budget (ADR 0013's 2026-07-17 amendment —
//! never a Direct fallback).
//!
//! There is exactly ONE frame loop: waiting for a warming owner reads the same
//! frames through the same loop as any other request.
//!
//! A failed request can leave the socket carrying frames the client never read —
//! a stalled request's late answer arrives after the stall verdict. A session is
//! therefore POISONED by any failure that leaves the stream's position unknown,
//! and the next use reconnects rather than reading a stale frame as the new
//! request's answer.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use norn_wire::{
    ApplyParams, ApplyReport, AuditParams, AuditReport, ClientFrame, CountParams, CountReport,
    DeleteParams, DescribeParams, DescribeReport, EditParams, EditReport, FindParams, FindReport,
    GetParams, GetReport, MoveParams, NewParams, NewReport, OwnerFrame, Progress, ProgressPhase,
    RepairParams, RepairReport, RewriteWikilinkParams, ServingState, SetParams, SetReport,
    ValidateParams, ValidateReport, CONTROL_PROTOCOL, PROGRESS_HEARTBEAT,
};

use crate::error::ClientError;
use crate::SummonConfig;

/// The service stall budget (ADR 0013): the maximum SILENCE the client tolerates
/// while one request is in flight. Not a call timeout — a healthy owner
/// heartbeats (`norn_wire::PROGRESS_HEARTBEAT`) while its long work runs, and
/// every read restarts the budget, so only an owner emitting nothing at all is
/// "hung".
///
/// Strictly it is a PER-READ budget: it is pushed onto the socket as
/// `SO_RCVTIMEO`, which restarts on every `read` that returns bytes, not on
/// every complete line. Against this owner the two are the same thing — it
/// writes each frame with a single `write_all` + `flush`, so a frame arrives
/// whole or the connection dies (a heartbeat write that cannot land inside
/// its own bound shuts the connection down rather than ever appending the
/// terminal frame onto an unknown partial prefix; the reader then sees EOF or
/// one truncated final line — junk is never mis-decoded as an answer) — and a
/// hypothetical
/// peer that dripped one byte per budget would be tolerated indefinitely
/// without ever completing a frame. That shape is unreachable from the owner
/// in this workspace and is not defended against.
pub const STALL_BUDGET: Duration = Duration::from_secs(5);

/// How often the readiness wait re-pings a not-yet-serving owner. Small enough
/// that a warm owner is observed immediately; the `warming` progress it feeds is
/// throttled separately (see [`OwnerSession::wait_until_ready`]).
const READY_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Where a session hands the in-flight [`Progress`] frames it reads.
///
/// The client never renders: it reports typed facts and a display layer decides
/// what (if anything) a user sees (invariant 4). The default sink discards, so
/// progress frames are consumed — and the silence budget reset — whether or not
/// any surface draws them.
pub trait ProgressSink: Send {
    /// One in-flight observation arrived.
    fn progress(&mut self, progress: &Progress);
    /// The request ended. Called once per request that emitted at least one
    /// observation, so a sink drawing a transient line knows when to erase it.
    fn finished(&mut self) {}
}

/// The default sink: consume and drop. A surface that wants progress installs
/// its own via [`OwnerSession::set_progress_sink`].
struct DiscardProgress;

impl ProgressSink for DiscardProgress {
    fn progress(&mut self, _progress: &Progress) {}
}

/// A live, proven connection to a summoned owner.
pub struct OwnerSession {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    socket: PathBuf,
    /// The inter-frame silence budget. A field (not the [`STALL_BUDGET`] const)
    /// so tests can shrink it to drive the stall path fast; production keeps the
    /// default. Kept in lockstep with the socket's read timeout — see
    /// [`apply_stall_budget`](OwnerSession::apply_stall_budget).
    stall_budget: Duration,
    /// Where in-flight [`Progress`] frames go. Never `None`: an uninstalled sink
    /// is [`DiscardProgress`], so the frame loop has no "is anyone listening"
    /// branch and consuming a frame is unconditional.
    progress: Box<dyn ProgressSink>,
    /// The config this session was summoned with, retained so the session can
    /// self-heal (re-summon-or-connect) when the owner goes away before Ready is
    /// first observed — the linux-backlog race (see [`crate::open`]). `None` for
    /// test sessions wrapped around a fake owner: they never reconnect.
    config: Option<SummonConfig>,
    /// Whether the socket's frame stream is at an UNKNOWN position — a failure
    /// left frames possibly still in flight for a request that already returned
    /// its verdict. The next [`request`](Self::request) reconnects instead of
    /// reading, so a stalled request's late answer can never be served to the
    /// caller as the NEXT request's answer.
    poisoned: bool,
}

/// The parsed proof-of-life a `ping` returns.
#[derive(Debug, Clone)]
pub struct Pong {
    pub version: String,
    pub build: Option<String>,
    pub pid: u32,
    pub serving: ServingState,
}

impl OwnerSession {
    /// Wrap a connected stream. Verifies the peer's credentials (defense in
    /// depth: the socket path is computable, so a squatter could be listening —
    /// refuse anything not served by our own uid, never fall through to it), then
    /// sets the per-request read timeout (stall budget).
    pub(crate) fn new(
        stream: UnixStream,
        socket: PathBuf,
        config: Option<SummonConfig>,
    ) -> Result<Self, ClientError> {
        verify_peer_uid(&stream)?;
        stream
            .set_read_timeout(Some(STALL_BUDGET))
            .map_err(ClientError::Io)?;
        let writer = stream.try_clone().map_err(ClientError::Io)?;
        Ok(Self {
            reader: BufReader::new(stream),
            writer,
            socket,
            stall_budget: STALL_BUDGET,
            progress: Box::new(DiscardProgress),
            config,
            poisoned: false,
        })
    }

    /// Install the sink the frame loop hands in-flight [`Progress`] frames to,
    /// replacing the discarding default. The session still consumes every
    /// progress frame either way — a sink only decides whether anything is done
    /// with one.
    pub fn set_progress_sink(&mut self, sink: Box<dyn ProgressSink>) {
        self.progress = sink;
    }

    /// Re-establish the connection: re-run summon-or-connect (which re-validates
    /// the runtime dir and re-checks the peer uid) and swap in the fresh
    /// reader/writer/socket. Used to self-heal an owner that went away before
    /// Ready, and to clear a poisoned session. Requires a retained config
    /// (production sessions always have one).
    ///
    /// A fresh socket is a fresh frame stream, so this is also the one place
    /// [`poisoned`](Self::poisoned) clears: nothing another request wrote can be
    /// waiting on it.
    fn reconnect(&mut self) -> Result<(), ClientError> {
        let config = self
            .config
            .clone()
            .ok_or_else(|| ClientError::OwnerUnavailable {
                socket: self.socket.clone(),
            })?;
        let (stream, socket) = crate::connect_or_summon(&config)?;
        // `new` re-runs the peer-uid check, so every reconnect is re-verified.
        let fresh = OwnerSession::new(stream, socket, Some(config))?;
        self.reader = fresh.reader;
        self.writer = fresh.writer;
        self.socket = fresh.socket;
        // The fresh stream carries the DEFAULT read timeout; re-apply this
        // session's budget so a shrunk (test) budget survives a reconnect and
        // the socket deadline never drifts from `stall_budget`.
        self.apply_stall_budget()?;
        self.poisoned = false;
        Ok(())
    }

    /// Push [`stall_budget`](Self::stall_budget) onto the socket as its per-read
    /// deadline. The two are one value: the socket timeout is what MAKES the
    /// budget inter-frame, because it restarts on every `read` — so they are set
    /// together and never separately.
    fn apply_stall_budget(&mut self) -> Result<(), ClientError> {
        self.reader
            .get_ref()
            .set_read_timeout(Some(self.stall_budget))
            .map_err(ClientError::Io)
    }

    /// Re-establish a live, ready connection after the held owner went away — the
    /// recovery path a long-lived session (the MCP stdio server) drives when a
    /// call fails PRE-SEND ([`ClientError::is_owner_gone_pre_send`]). Re-runs
    /// summon-or-connect (spawning a fresh owner if the old one idle-reaped), then
    /// waits for it to warm up to Ready within `max_wait`. The CLI never needs
    /// this — it opens a fresh session per invocation — but a process that HOLDS a
    /// session across the owner's idle-TTL must resummon or every later call fails.
    pub fn resummon(&mut self, max_wait: Duration) -> Result<(), ClientError> {
        self.reconnect()?;
        self.wait_until_ready(max_wait)?;
        Ok(())
    }

    /// Test-only: shrink the inter-frame silence budget so the stall path is
    /// drivable without a multi-second wait. Sets the socket deadline with it —
    /// the budget IS the socket's per-read timeout.
    #[cfg(test)]
    pub(crate) fn set_stall_budget(&mut self, budget: Duration) {
        self.stall_budget = budget;
        self.apply_stall_budget()
            .expect("setting a read timeout on a live test socket cannot fail");
    }

    /// The socket this session is bound to.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Liveness + serving-state probe.
    ///
    /// A warm-up that failed on an invalid `.norn/config.yaml` answers every
    /// frame — including a ping — with the config error as a `Rejected`
    /// (NRN-360). The owner is healthy (not exit-to-heal), so [`unexpected_frame`]
    /// rides it onto the user-error path ([`ClientError::Rejected`]):
    /// `wait_until_ready` returns it and the CLI renders the config error, never
    /// a resummon/crash loop.
    pub fn ping(&mut self) -> Result<Pong, ClientError> {
        match self.request(&ClientFrame::Ping {
            protocol: CONTROL_PROTOCOL,
        })? {
            OwnerFrame::Pong {
                version,
                build,
                pid,
                serving,
                ..
            } => Ok(Pong {
                version,
                build,
                pid,
                serving,
            }),
            other => Err(self.unexpected(other, "pong")),
        }
    }

    /// The trivial routed read: the vault's document count (NRN-345 stand-in).
    pub fn probe(&mut self) -> Result<u64, ClientError> {
        match self.request(&ClientFrame::Probe)? {
            OwnerFrame::Probe { document_count } => Ok(document_count),
            other => Err(self.unexpected(other, "probe report")),
        }
    }

    /// Run a `find` request against the owner's warm cache.
    pub fn find(&mut self, params: FindParams) -> Result<FindReport, ClientError> {
        match self.request(&ClientFrame::Find { params })? {
            OwnerFrame::Find { report } => Ok(report),
            other => Err(self.unexpected(other, "find report")),
        }
    }

    /// Run a `count` request against the owner's warm cache.
    pub fn count(&mut self, params: CountParams) -> Result<CountReport, ClientError> {
        match self.request(&ClientFrame::Count { params })? {
            OwnerFrame::Count { report } => Ok(report),
            other => Err(self.unexpected(other, "count report")),
        }
    }

    /// Run a `get` request against the owner's warm cache.
    pub fn get(&mut self, params: GetParams) -> Result<GetReport, ClientError> {
        match self.request(&ClientFrame::Get { params })? {
            OwnerFrame::Get { report } => Ok(report),
            other => Err(self.unexpected(other, "get report")),
        }
    }

    /// Run a `describe` request against the owner's warm cache + retained config.
    pub fn describe(&mut self, params: DescribeParams) -> Result<DescribeReport, ClientError> {
        match self.request(&ClientFrame::Describe { params })? {
            OwnerFrame::Describe { report } => Ok(report),
            other => Err(self.unexpected(other, "describe report")),
        }
    }

    /// Run a `validate` request against the owner's warm graph + retained config.
    pub fn validate(&mut self, params: ValidateParams) -> Result<ValidateReport, ClientError> {
        match self.request(&ClientFrame::Validate { params })? {
            OwnerFrame::Validate { report } => Ok(report),
            other => Err(self.unexpected(other, "validate report")),
        }
    }

    /// Run a `repair` request against the owner's warm graph + retained config.
    /// Read-only: the owner builds the findings-derived `MigrationPlan` and never
    /// writes (the returned plan is the output; `apply` executes it).
    pub fn repair(&mut self, params: RepairParams) -> Result<RepairReport, ClientError> {
        match self.request(&ClientFrame::Repair { params })? {
            OwnerFrame::Repair { report } => Ok(report),
            other => Err(self.unexpected(other, "repair report")),
        }
    }

    /// Run an `audit` read against the owner's durable mutation event stream.
    /// Read-only: the OWNER reads the per-vault JSONL store (it is co-located
    /// with the vault's state home; an off-filesystem client could not) and
    /// returns the newest-first matched events. An empty stream yields an empty
    /// report, never an error.
    pub fn audit(&mut self, params: AuditParams) -> Result<AuditReport, ClientError> {
        match self.request(&ClientFrame::Audit { params })? {
            OwnerFrame::Audit { report } => Ok(report),
            other => Err(self.unexpected(other, "audit report")),
        }
    }

    /// Run a `set` mutation against the owner's warm cache. A single request:
    /// the owner serializes writes under its in-process single-writer lock, and
    /// a post-send failure is NOT retried here (no resummon loop) — a mutation
    /// that may have applied must not double-apply (ADR 0011). A clean pre-write
    /// decline arrives as a report with `outcome = refused`, not an error.
    pub fn set(&mut self, params: SetParams) -> Result<SetReport, ClientError> {
        match self.request(&ClientFrame::Set { params })? {
            OwnerFrame::Set { report } => Ok(report),
            other => Err(self.unexpected(other, "set report")),
        }
    }

    /// Run a `new` mutation against the owner's warm cache. Same send-once,
    /// never-retry contract as [`set`](Self::set).
    pub fn new_document(&mut self, params: NewParams) -> Result<NewReport, ClientError> {
        match self.request(&ClientFrame::New { params })? {
            OwnerFrame::New { report } => Ok(report),
            other => Err(self.unexpected(other, "new report")),
        }
    }

    /// Run an `edit` mutation against the owner's warm cache. Same send-once,
    /// never-retry contract as [`set`](Self::set).
    pub fn edit(&mut self, params: EditParams) -> Result<EditReport, ClientError> {
        match self.request(&ClientFrame::Edit { params })? {
            OwnerFrame::Edit { report } => Ok(report),
            other => Err(self.unexpected(other, "edit report")),
        }
    }

    /// Run a `move` mutation against the owner's warm cache. Same send-once,
    /// never-retry contract as [`set`](Self::set). The report is the shared typed
    /// [`ApplyReport`] (which lives in `norn-wire`), consumed directly by the CLI.
    pub fn move_document(&mut self, params: MoveParams) -> Result<ApplyReport, ClientError> {
        match self.request(&ClientFrame::Move { params })? {
            OwnerFrame::Move { report } => Ok(report),
            other => Err(self.unexpected(other, "move report")),
        }
    }

    /// Run a `delete` mutation against the owner's warm cache. Same contract as
    /// [`move_document`](Self::move_document).
    pub fn delete(&mut self, params: DeleteParams) -> Result<ApplyReport, ClientError> {
        match self.request(&ClientFrame::Delete { params })? {
            OwnerFrame::Delete { report } => Ok(report),
            other => Err(self.unexpected(other, "delete report")),
        }
    }

    /// Run a `rewrite-wikilink` mutation against the owner's warm cache. Same
    /// contract as [`move_document`](Self::move_document).
    pub fn rewrite_wikilink(
        &mut self,
        params: RewriteWikilinkParams,
    ) -> Result<ApplyReport, ClientError> {
        match self.request(&ClientFrame::RewriteWikilink { params })? {
            OwnerFrame::RewriteWikilink { report } => Ok(report),
            other => Err(self.unexpected(other, "rewrite-wikilink report")),
        }
    }

    /// Run an `apply` mutation — execute an already-reviewed `MigrationPlan`
    /// (carried typed in `params.plan`) — against the owner's warm cache. Same
    /// send-once, never-retry contract as [`move_document`](Self::move_document);
    /// the report is the shared typed [`ApplyReport`] the CLI consumes directly.
    pub fn apply(&mut self, params: ApplyParams) -> Result<ApplyReport, ClientError> {
        match self.request(&ClientFrame::Apply { params })? {
            OwnerFrame::Apply { report } => Ok(report),
            other => Err(self.unexpected(other, "apply report")),
        }
    }

    /// Ping until the owner reports [`ServingState::Ready`], honoring ADR 0013's
    /// liveness contract and 0017's accepted warm-up cost.
    ///
    /// **This is not a second liveness mechanism.** Each ping runs through the
    /// one frame loop ([`request`](Self::request)), so the inter-frame silence
    /// budget is the whole hung-owner verdict here as everywhere: a warming
    /// owner keeps answering, and one that says nothing for a budget surfaces as
    /// [`ClientError::OwnerHealth`] from inside [`ping`](Self::ping). Warm-up is
    /// healthy however long it takes (~linear in vault size, 0017's accepted
    /// cost) — this loop only bounds the wait by `max_wait`.
    ///
    /// The pong carries no progress counter at all: an owner reports progress by
    /// emitting frames, not by advancing a number a poller inspects, so there is
    /// nothing here to sample between polls.
    ///
    /// **The wait itself feeds the progress sink.** A pong whose
    /// [`ServingState`] is not `Ready` IS the owner saying, in a typed fact, that
    /// it is still warming — so this hands the sink a `warming` observation,
    /// throttled to at most one per `PROGRESS_HEARTBEAT` rather than one per
    /// poll. Nothing is invented: no pong, no observation. The sink is closed out
    /// (once) on the way out, so a surface drawing a transient line erases it
    /// whether the wait ended in Ready or in an error.
    ///
    /// **The throttle clock is seeded at the WAIT's start, not at the first
    /// not-Ready pong.** A warm-up that finishes inside one `PROGRESS_HEARTBEAT`
    /// therefore draws nothing at all — matching the owner emitter's own
    /// cadence (it does not heartbeat a request that finishes inside its first
    /// interval either), rather than flashing a warming line for a wait that
    /// was never actually slow.
    ///
    /// Verbs still send nothing before Ready. That gate is load-bearing: a verb
    /// frame written pre-Ready would convert an owner that exits to heal
    /// mid-warm-up from a clean pre-send resummon into ADR 0011's post-send
    /// uncertainty — for work that never ran.
    ///
    /// Before Ready is first observed, an owner that goes away at the connection
    /// level ([`ClientError::OwnerGone`]) — the linux drain-window backlog race
    /// (see [`crate::open`]) — is self-healed by re-summoning (bounded by
    /// `max_wait`), never surfaced as raw IO. After Ready would be observed the
    /// method has returned, so a mid-request drop on the returned session stays a
    /// hard error (post-send uncertainty is a separate contract).
    pub fn wait_until_ready(&mut self, max_wait: Duration) -> Result<Pong, ClientError> {
        let start = Instant::now();
        // The throttle clock, SEEDED AT WAIT START rather than at the first
        // not-Ready pong: the first draw only happens once a full
        // `PROGRESS_HEARTBEAT` of continuous not-Ready has elapsed, so a
        // warm-up finishing inside that interval draws nothing (F4).
        let mut last_drawn_at = start;
        // Separate from the throttle clock above: `last_drawn_at` is always
        // seeded to a real `Instant`, so it cannot itself answer "did this
        // wait draw anything" the way an `Option` could. The closing
        // `finished` call is gated on this instead.
        let mut drawn = false;
        // One exit point, so the sink is closed out on EVERY way out — Ready,
        // timeout, or a surfaced error — rather than at four `return`s.
        let outcome = loop {
            let pong = match self.ping() {
                Ok(pong) => pong,
                // Owner went away before Ready — resummon and retry, bounded by
                // `max_wait`. A hung owner (OwnerHealth) or other error is NOT
                // healable this way, so it surfaces.
                Err(e) if e.is_owner_gone() => {
                    if start.elapsed() > max_wait {
                        break Err(e);
                    }
                    match self.reconnect() {
                        Ok(()) => continue,
                        Err(e) => break Err(e),
                    }
                }
                Err(e) => break Err(e),
            };
            if pong.serving == ServingState::Ready {
                break Ok(pong);
            }
            // The owner just reported, as a typed serving state, that it is not
            // serving yet: that is the `warming` fact, read off the pong rather
            // than assumed from elapsed time. Throttled to the heartbeat floor —
            // a 20ms poll cadence would redraw a progress line 50 times a second
            // to say the same thing — and the clock started at the WAIT's own
            // start, so the first draw waits out a full interval too.
            if last_drawn_at.elapsed() >= PROGRESS_HEARTBEAT {
                last_drawn_at = Instant::now();
                drawn = true;
                self.progress
                    .progress(&Progress::new(ProgressPhase::Warming));
            }
            if start.elapsed() > max_wait {
                break Err(ClientError::OwnerHealth(
                    "timed out waiting for the owner to become ready".to_string(),
                ));
            }
            std::thread::sleep(READY_POLL_INTERVAL);
        };
        if drawn {
            self.progress.finished();
        }
        outcome
    }

    /// Send one frame and read the request's stream to its terminal frame.
    ///
    /// The ONE frame loop (NRN-512). The loop returns exactly on
    /// [`OwnerFrame::is_terminal`]; a non-terminal frame is consumed as proof of
    /// life (a [`Progress`] hands its observation to the sink) and the loop reads
    /// again. The socket's read deadline is
    /// [`stall_budget`](Self::stall_budget), and a read deadline restarts per
    /// `read` call — which is precisely what makes the budget a silence budget
    /// rather than a whole-call timeout. A mutation that runs for a minute but
    /// heartbeats every second is healthy; an owner that says nothing for a whole
    /// budget is hung.
    ///
    /// Post-send failure shapes are unchanged (ADR 0011): EOF mid-stream is
    /// [`ClientError::OwnerGone`] — the request WAS written, so a mutation may
    /// have applied and no caller may blind-retry it — and a silence timeout is
    /// [`ClientError::OwnerHealth`].
    ///
    /// A verdict reached while the owner may still be writing leaves the stream
    /// at an unknown position, so it POISONS the session (see
    /// [`desynchronizes`]) and the next request reconnects first.
    fn request(&mut self, frame: &ClientFrame) -> Result<OwnerFrame, ClientError> {
        // A poisoned socket may still deliver the previous request's late
        // frames; reading them here would answer THIS request with the last
        // one's report. A held session (the MCP server) is where that bites, so
        // the recovery is a fresh connection, not a hopeful re-read.
        if self.poisoned {
            self.reconnect()?;
        }
        let mut line = serde_json::to_vec(frame)
            .map_err(|e| ClientError::Protocol(format!("failed to encode frame: {e}")))?;
        line.push(b'\n');
        // A write to a peer that already went away fails at the connection level
        // (BrokenPipe/ConnectionReset). This is the PRE-SEND shape: the frame was
        // never delivered, so it is safe to resummon and retry (even a mutation).
        // A held owner that idle-reaped between calls fails HERE on the first
        // write — the recovery seam a long-lived session (MCP) heals from.
        //
        // A non-connection `Io` error is different: `write_all` can fail after
        // committing a PARTIAL line to the socket, which desyncs the OWNER's
        // line-oriented parser the same way an undecodable read does for THIS
        // side (see [`desynchronizes`]) — so only that arm poisons the session.
        // `OwnerGonePreSend` stays clean: that shape means nothing reached the
        // socket at all, so its safe-retry contract is unaffected.
        if let Err(e) = self.writer.write_all(&line) {
            let err = classify_io_pre_send(e);
            if matches!(err, ClientError::Io(_)) {
                self.poisoned = true;
            }
            return Err(err);
        }
        self.writer.flush().map_err(classify_io_pre_send)?;

        let mut observed_progress = false;
        let mut resp = String::new();
        loop {
            resp.clear();
            let outcome = match self.reader.read_line(&mut resp) {
                // EOF before the terminal frame == the owner exited mid-exchange
                // (the drain-window shape) — a resummon signal, not a hang.
                Ok(0) => Err(ClientError::OwnerGone(
                    "owner closed the connection before replying".to_string(),
                )),
                Ok(_) => serde_json::from_str::<OwnerFrame>(resp.trim())
                    .map_err(|e| ClientError::Protocol(format!("undecodable owner frame: {e}"))),
                // Silence past the budget with no frame of any kind: hung.
                Err(e) if is_timeout(&e) => Err(ClientError::OwnerHealth(
                    "no frame from owner within the stall budget".to_string(),
                )),
                Err(e) => Err(classify_io(e)),
            };
            match outcome {
                // Not the end of the request: consume it and read on. The frame
                // arriving is itself the proof of life that restarts the budget;
                // `Progress` additionally carries an observation to report.
                Ok(frame) if !frame.is_terminal() => {
                    if let OwnerFrame::Progress { progress } = &frame {
                        observed_progress = true;
                        self.progress.progress(progress);
                    }
                }
                other => {
                    if observed_progress {
                        self.progress.finished();
                    }
                    if let Err(err) = &other {
                        self.poisoned = desynchronizes(err);
                    }
                    return other;
                }
            }
        }
    }

    /// [`unexpected_frame`], additionally poisoning the session on a genuine
    /// protocol mismatch (NRN-512 delta): a terminal frame of a kind NEITHER
    /// the verb's own success frame NOR one of the owner's two normal
    /// cross-verb error paths — the owner answered a `find` with a `pong`,
    /// say — means this reader is one frame behind where the owner's writer
    /// thinks it is. That is the same "stream position now unknown" class
    /// [`desynchronizes`] already names for the read-path failures in
    /// [`request`](Self::request); the difference is only that this verdict is
    /// reached by matching a frame's *kind* rather than by a read failing
    /// outright.
    ///
    /// `Rejected` (a warm-up config error) and `Error` are NOT this: they are
    /// well-formed terminal answers the owner sends deliberately, on ANY
    /// request, when that request cannot be served — the stream is exactly
    /// where the reader expects it, just carrying a different (still
    /// understood) frame kind. Poisoning on those would force every caller
    /// through a needless reconnect for a perfectly healthy connection — see
    /// `ping_maps_a_rejected_config_error_onto_the_user_error_path`.
    ///
    /// Every verb method routes its non-success arm through this instead of
    /// the free fn directly, so the poison can never be forgotten at a new
    /// call site.
    fn unexpected(&mut self, frame: OwnerFrame, expected: &str) -> ClientError {
        let err = unexpected_frame(frame, expected);
        if matches!(err, ClientError::Protocol(_)) {
            self.poisoned = true;
        }
        err
    }
}

/// Map a reply that is NOT a verb's own success frame onto a client error —
/// every verb method reaches this through [`OwnerSession::unexpected`]
/// (NRN-411 / the NRN-512 poisoning delta), never directly. A warm-up/user
/// [`OwnerFrame::Rejected`] rides the user-error path ([`ClientError::Rejected`],
/// carrying the message + hints); an [`OwnerFrame::Error`] becomes an
/// [`ClientError::OwnerError`]; any other frame is a protocol mismatch labelled
/// with `expected` (e.g. `"find report"`). A free fn, not a `request`-wrapping
/// closure, so no verb closure returns the large `OwnerFrame` in an `Err`
/// (which `clippy::result_large_err` would flag).
fn unexpected_frame(frame: OwnerFrame, expected: &str) -> ClientError {
    match frame {
        OwnerFrame::Rejected { message, hints } => ClientError::Rejected { message, hints },
        OwnerFrame::Error { message } => ClientError::OwnerError(message),
        other => ClientError::Protocol(format!("expected {expected}, got {other:?}")),
    }
}

/// Whether a failed request leaves the socket's frame stream at an UNKNOWN
/// position — the session is poisoned and must reconnect before its next use.
///
/// - [`OwnerHealth`](ClientError::OwnerHealth): the client gave up on silence,
///   but the owner was never told. Its terminal frame (and any further progress)
///   can still arrive, and a HELD session reusing the socket would read that
///   late answer as the NEXT request's — a `find` for B returning A's report,
///   silently and with a clean exit code.
/// - [`Protocol`](ClientError::Protocol): an undecodable line means the stream
///   is not where the reader thinks it is.
/// - [`Io`](ClientError::Io): a read that failed for a non-connection reason
///   consumed an unknown number of bytes.
///
/// The connection-level shapes are NOT poison: that socket is dead, so there is
/// nothing stale to read from it, and both are already a resummon signal
/// ([`ClientError::is_owner_gone`]) with their own ADR 0011 retry contract.
fn desynchronizes(err: &ClientError) -> bool {
    match err {
        ClientError::OwnerHealth(_) | ClientError::Protocol(_) | ClientError::Io(_) => true,
        ClientError::OwnerGone(_)
        | ClientError::OwnerGonePreSend(_)
        | ClientError::OwnerUnavailable { .. }
        | ClientError::ForeignOwner { .. }
        | ClientError::OwnerError(_)
        | ClientError::Rejected { .. }
        | ClientError::Resolve(_)
        | ClientError::NoRuntimeDir
        | ClientError::InsecureRuntimeDir(_)
        | ClientError::Spawn { .. } => false,
    }
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// Whether an IO error is a connection-level drop (the owner went away) rather
/// than a genuine transport IO fault.
fn is_connection_level(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(
        e.kind(),
        BrokenPipe | ConnectionReset | ConnectionAborted | UnexpectedEof | NotConnected
    )
}

/// Map a READ-path socket IO error to [`ClientError::OwnerGone`] (POST-SEND — the
/// request was already written) when it is a connection-level drop, else a raw
/// [`ClientError::Io`]. Post-send: a caller must NOT blind-retry (ADR 0011).
fn classify_io(e: std::io::Error) -> ClientError {
    if is_connection_level(&e) {
        ClientError::OwnerGone(e.to_string())
    } else {
        ClientError::Io(e)
    }
}

/// Map a WRITE-path socket IO error to [`ClientError::OwnerGonePreSend`] (the
/// frame was never delivered — safe to resummon and retry) when it is a
/// connection-level drop, else a raw [`ClientError::Io`].
fn classify_io_pre_send(e: std::io::Error) -> ClientError {
    if is_connection_level(&e) {
        ClientError::OwnerGonePreSend(e.to_string())
    } else {
        ClientError::Io(e)
    }
}

/// Try a single connect to `socket`. `None` (via `Err`) simply means no owner is
/// listening yet.
pub(crate) fn connect(socket: &Path) -> std::io::Result<UnixStream> {
    UnixStream::connect(socket)
}

/// Connect with bounded retry/backoff — the owner needs a moment to bind after
/// being spawned. Returns [`ClientError::OwnerUnavailable`] if nothing binds
/// within `budget`.
pub(crate) fn connect_with_retry(
    socket: &Path,
    budget: Duration,
) -> Result<UnixStream, ClientError> {
    let start = Instant::now();
    let mut backoff = Duration::from_millis(5);
    loop {
        match connect(socket) {
            Ok(stream) => return Ok(stream),
            Err(_) if start.elapsed() < budget => {
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_millis(100));
            }
            Err(_) => {
                return Err(ClientError::OwnerUnavailable {
                    socket: socket.to_path_buf(),
                })
            }
        }
    }
}

/// The peer's uid on a connected Unix stream. `getpeereid` is a BSD/macOS API;
/// Linux has no such libc symbol and exposes peer credentials through
/// `getsockopt(SOL_SOCKET, SO_PEERCRED)` instead — the two cfg arms below are
/// the same check on each platform's native surface.
#[cfg(not(target_os = "linux"))]
fn peer_uid(stream: &UnixStream) -> Result<u32, ClientError> {
    use std::os::unix::io::AsRawFd;
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: getpeereid reads the peer credentials of a connected AF_UNIX socket
    // into two valid local out-params; no aliasing, no ownership transfer.
    #[allow(unsafe_code)]
    let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if rc != 0 {
        return Err(ClientError::Io(std::io::Error::last_os_error()));
    }
    Ok(uid as u32)
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> Result<u32, ClientError> {
    use std::os::unix::io::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: SO_PEERCRED on a connected AF_UNIX socket fills a ucred struct;
    // the out-params are valid locals sized by `len`; no aliasing, no ownership
    // transfer.
    #[allow(unsafe_code)]
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::addr_of_mut!(cred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(ClientError::Io(std::io::Error::last_os_error()));
    }
    Ok(cred.uid)
}

/// Reject a peer whose uid differs from ours. Split from the syscall so the deny
/// branch is unit-testable without a privileged foreign listener.
fn check_peer_uid(peer_uid: u32, our_uid: u32) -> Result<(), ClientError> {
    if peer_uid == our_uid {
        Ok(())
    } else {
        Err(ClientError::ForeignOwner {
            peer_uid,
            expected_uid: our_uid,
        })
    }
}

/// Verify the socket's peer runs as our uid; a foreign owner is refused.
fn verify_peer_uid(stream: &UnixStream) -> Result<(), ClientError> {
    check_peer_uid(peer_uid(stream)?, crate::addr::current_uid())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Write};

    #[test]
    fn check_peer_uid_allows_same_and_rejects_foreign() {
        assert!(check_peer_uid(1000, 1000).is_ok());
        match check_peer_uid(4242, 1000) {
            Err(ClientError::ForeignOwner {
                peer_uid,
                expected_uid,
            }) => {
                assert_eq!(peer_uid, 4242);
                assert_eq!(expected_uid, 1000);
            }
            other => panic!("expected ForeignOwner, got {other:?}"),
        }
    }

    /// A scripted fake owner: binds `socket`, accepts one connection, and answers
    /// every client frame with `answer(started_at)`. Exits when the client
    /// disconnects. Drives the client's liveness logic deterministically without
    /// a real build.
    fn fake_owner(
        socket: std::path::PathBuf,
        answer: impl Fn(Instant) -> OwnerFrame + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let started = Instant::now();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let mut buf = serde_json::to_vec(&answer(started)).unwrap();
                buf.push(b'\n');
                if writer.write_all(&buf).is_err() || writer.flush().is_err() {
                    break;
                }
            }
        })
    }

    fn pong(serving: ServingState) -> OwnerFrame {
        OwnerFrame::Pong {
            protocol: CONTROL_PROTOCOL,
            version: "0.0.0".into(),
            build: None,
            pid: 1,
            serving,
        }
    }

    fn connected_session(socket: &std::path::Path) -> OwnerSession {
        let stream = UnixStream::connect(socket).unwrap();
        // No config: these fake-owner sessions never reconnect.
        OwnerSession::new(stream, socket.to_path_buf(), None).unwrap()
    }

    /// The linux drain-window shape, made deterministic on every platform: an
    /// owner that accepts one connection, reads the client's frame, then closes
    /// WITHOUT replying (as the reaper's listener-drop + process exit does to a
    /// connection sitting in the accept backlog). The first exchange must
    /// classify as `OwnerGone` (the resummon signal) — never a raw IO error or a
    /// spurious pong.
    #[test]
    fn owner_closing_after_accept_classifies_as_owner_gone() {
        use std::io::Read;
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("closer.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Consume the client's ping frame, then drop the connection.
            let mut buf = [0u8; 256];
            let _ = stream.read(&mut buf);
            drop(stream);
        });

        let mut session = connected_session(&socket);
        let err = session
            .ping()
            .expect_err("a closed-after-accept owner must not yield a pong");
        assert!(err.is_owner_gone(), "expected OwnerGone, got {err:?}");

        handle.join().unwrap();
    }

    /// NRN-360: an owner whose warm-up failed on an invalid config answers a
    /// ping with a `Rejected` carrying the config message. `ping` must surface
    /// that as [`ClientError::Rejected`] (the user-error path), NOT a protocol
    /// error and NOT owner-gone — so `wait_until_ready` returns it verbatim
    /// instead of resummoning into a crash loop.
    #[test]
    fn ping_maps_a_rejected_config_error_onto_the_user_error_path() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("badconfig.sock");
        let handle = fake_owner(socket.clone(), |_| OwnerFrame::Rejected {
            message: "invalid config /vault/.norn/config.yaml: unknown field `bogus`".to_string(),
            hints: Vec::new(),
        });

        // One connection (the fake owner serves a single accept): a direct ping
        // and a `wait_until_ready` poll must BOTH surface the Rejected.
        let mut session = connected_session(&socket);
        let err = session
            .ping()
            .expect_err("a bad-config owner must reject the ping");
        match &err {
            ClientError::Rejected { message, .. } => {
                assert!(
                    message.starts_with("invalid config "),
                    "expected the `invalid config` message, got {message:?}"
                );
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
        assert!(
            !err.is_owner_gone(),
            "a config error is not a resummon signal"
        );

        // wait_until_ready pings in a loop; a Rejected must be returned as-is,
        // never triggering the owner-gone resummon path.
        let err = session
            .wait_until_ready(Duration::from_secs(5))
            .expect_err("a bad-config owner never reaches ready");
        assert!(matches!(err, ClientError::Rejected { .. }), "got {err:?}");

        drop(session);
        handle.join().unwrap();
    }

    /// Finding 1: a warm-up (`opening`) that runs LONGER than the stall budget
    /// must still reach `ready` — it is healthy liveness, not a hang. The
    /// verdict is keyed on FRAMES arriving, which is why an owner that keeps
    /// answering is alive no matter how long the build takes; there is no
    /// progress counter a poller could declare frozen (NRN-512 deleted the
    /// pong's `writer_progress`, so the sequence-stall heuristic it fed has no
    /// wire representation left to test).
    #[test]
    fn warmup_longer_than_stall_budget_still_reaches_ready() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("warmup.sock");
        // `opening` for 150ms (>> the 50ms budget below), then ready.
        let handle = fake_owner(socket.clone(), |started| {
            if started.elapsed() < Duration::from_millis(150) {
                pong(ServingState::Opening)
            } else {
                pong(ServingState::Ready)
            }
        });

        let mut session = connected_session(&socket);
        session.set_stall_budget(Duration::from_millis(50));
        let got = session
            .wait_until_ready(Duration::from_secs(5))
            .expect("a long non-busy warm-up must not be declared hung");
        assert_eq!(got.serving, ServingState::Ready);

        drop(session);
        handle.join().unwrap();
    }

    /// A recording sink: keeps every observation the frame loop handed it plus
    /// the finish count, so a test can assert both that progress was consumed
    /// and that the request was closed out exactly once.
    #[derive(Clone, Default)]
    struct RecordingSink(std::sync::Arc<std::sync::Mutex<(Vec<Progress>, usize)>>);

    impl RecordingSink {
        fn observations(&self) -> Vec<Progress> {
            self.0.lock().unwrap().0.clone()
        }
        fn finishes(&self) -> usize {
            self.0.lock().unwrap().1
        }
    }

    impl ProgressSink for RecordingSink {
        fn progress(&mut self, progress: &Progress) {
            self.0.lock().unwrap().0.push(*progress);
        }
        fn finished(&mut self) {
            self.0.lock().unwrap().1 += 1;
        }
    }

    /// A fake owner that answers one client frame with `heartbeats` progress
    /// frames spaced `every` apart, then `terminal`. Models a long request that
    /// keeps its client informed.
    fn heartbeating_owner(
        socket: std::path::PathBuf,
        heartbeats: usize,
        every: Duration,
        terminal: OwnerFrame,
    ) -> std::thread::JoinHandle<()> {
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            let mut line = String::new();
            let write = |frame: &OwnerFrame, w: &mut UnixStream| -> bool {
                let mut buf = serde_json::to_vec(frame).unwrap();
                buf.push(b'\n');
                w.write_all(&buf).is_ok() && w.flush().is_ok()
            };
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                for i in 0..heartbeats {
                    std::thread::sleep(every);
                    let frame = OwnerFrame::Progress {
                        progress: Progress::new(norn_wire::ProgressPhase::Applying)
                            .with_done(i as u64 + 1)
                            .with_total(Some(heartbeats as u64)),
                    };
                    if !write(&frame, &mut writer) {
                        return;
                    }
                }
                if !write(&terminal, &mut writer) {
                    return;
                }
            }
        })
    }

    /// THE regression (NRN-512): a mutation whose total wall time far exceeds
    /// the silence budget must SURVIVE when heartbeats flow. Before the framed
    /// protocol, the flat per-request read deadline abandoned it post-send —
    /// straight into ADR 0011's no-safe-retry uncertainty — even though the
    /// owner was healthy and still working.
    #[test]
    fn a_mutation_outliving_the_silence_budget_survives_on_heartbeats() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("long-mutation.sock");
        // 6 heartbeats × 30ms ≈ 180ms of work against a 50ms budget: more than
        // three whole budgets, with no gap ever reaching one.
        let handle = heartbeating_owner(
            socket.clone(),
            6,
            Duration::from_millis(30),
            OwnerFrame::Probe { document_count: 42 },
        );

        let mut session = connected_session(&socket);
        session.set_stall_budget(Duration::from_millis(50));
        let sink = RecordingSink::default();
        session.set_progress_sink(Box::new(sink.clone()));

        let count = session
            .probe()
            .expect("heartbeats must hold the request open past the silence budget");
        assert_eq!(count, 42);

        let observed = sink.observations();
        assert_eq!(observed.len(), 6, "every progress frame reaches the sink");
        assert_eq!(observed[0].done, Some(1));
        assert_eq!(observed[5].total, Some(6));
        assert_eq!(sink.finishes(), 1, "the request closes the sink out once");

        drop(session);
        handle.join().unwrap();
    }

    /// The other half of the contract: an owner that emits NOTHING — no
    /// progress, no terminal — is genuinely wedged and still earns the stall
    /// verdict at the budget. Heartbeats buy tolerance; silence does not.
    #[test]
    fn a_silent_owner_is_owner_health_at_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("wedged.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            // Consume the request and then say nothing at all, holding the
            // connection open (never EOF) — the wedged-owner shape.
            let _ = reader.read_line(&mut line);
            std::thread::sleep(Duration::from_millis(400));
        });

        let mut session = connected_session(&socket);
        session.set_stall_budget(Duration::from_millis(50));
        let started = Instant::now();
        let err = session
            .probe()
            .expect_err("an owner emitting no frames at all is hung");
        assert!(matches!(err, ClientError::OwnerHealth(_)), "got {err:?}");
        assert!(
            started.elapsed() < Duration::from_millis(350),
            "the verdict must land at the budget, not at the owner's own timeout"
        );

        drop(session);
        handle.join().unwrap();
    }

    /// Warm-up progress rides the SAME frames as any other in-flight work
    /// (NRN-512's one-emitter rule): a request PARKED behind warm-up is answered
    /// with `warming` progress frames and then its terminal frame, through the
    /// one frame loop — no pre-Ready special path. The frames carry no units,
    /// because a build publishes no count the owner already holds.
    #[test]
    fn warm_up_progress_rides_the_same_frames() {
        use norn_wire::ProgressPhase;
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("warming.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            for _ in 0..3 {
                std::thread::sleep(Duration::from_millis(30));
                let frame = OwnerFrame::Progress {
                    progress: Progress::new(ProgressPhase::Warming),
                };
                let mut buf = serde_json::to_vec(&frame).unwrap();
                buf.push(b'\n');
                writer.write_all(&buf).unwrap();
                writer.flush().unwrap();
            }
            let mut buf = serde_json::to_vec(&OwnerFrame::Probe {
                document_count: 300,
            })
            .unwrap();
            buf.push(b'\n');
            writer.write_all(&buf).unwrap();
            writer.flush().unwrap();
        });

        let mut session = connected_session(&socket);
        session.set_stall_budget(Duration::from_millis(50));
        let sink = RecordingSink::default();
        session.set_progress_sink(Box::new(sink.clone()));

        assert_eq!(session.probe().expect("a warming owner is not hung"), 300);
        let observed = sink.observations();
        assert_eq!(observed.len(), 3);
        assert!(
            observed.iter().all(|p| p.phase == ProgressPhase::Warming),
            "warm-up progress is tagged `warming`: {observed:?}"
        );
        assert!(
            observed
                .iter()
                .all(|p| p.done.is_none() && p.total.is_none()),
            "a warming frame reports the phase alone: {observed:?}"
        );

        drop(session);
        handle.join().unwrap();
    }

    /// The readiness wait is the longest thing a first invocation waits on, and
    /// it feeds the sink itself (NRN-512): a pong that reports a not-yet-serving
    /// state IS the `warming` fact. Nothing is invented — the observation is
    /// derived from the pong's typed `serving`, and it is throttled to the
    /// heartbeat floor rather than emitted per 20ms poll. The throttle clock is
    /// seeded at the wait's own start (F4), so this warm-up must outlast one
    /// full `PROGRESS_HEARTBEAT` to draw anything at all — see the sub-interval
    /// counterpart below for the "finishes inside the interval" case.
    #[test]
    fn the_readiness_wait_reports_warming_from_the_pongs_serving_state() {
        use norn_wire::ProgressPhase;
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("warming-wait.sock");
        // `opening` for 1.7s — comfortably past the one-second heartbeat
        // floor and still short of the 2.0s second-draw boundary, so the
        // wait's first draw fires once and the owner is Ready shortly after.
        let handle = fake_owner(socket.clone(), |started| {
            if started.elapsed() < Duration::from_millis(1700) {
                pong(ServingState::Opening)
            } else {
                pong(ServingState::Ready)
            }
        });

        let mut session = connected_session(&socket);
        let sink = RecordingSink::default();
        session.set_progress_sink(Box::new(sink.clone()));

        let got = session.wait_until_ready(Duration::from_secs(5)).unwrap();
        assert_eq!(got.serving, ServingState::Ready);

        let observed = sink.observations();
        assert_eq!(
            observed.len(),
            1,
            "one observation per heartbeat floor, not one per poll: {observed:?}"
        );
        assert_eq!(observed[0].phase, ProgressPhase::Warming);
        assert_eq!(
            (observed[0].done, observed[0].total),
            (None, None),
            "a warming observation is the phase alone"
        );
        assert_eq!(
            sink.finishes(),
            1,
            "the wait closes the sink out, so a transient line is erased"
        );

        drop(session);
        handle.join().unwrap();
    }

    /// A warm owner answers `ready` on the first ping, so the wait draws
    /// nothing at all — an already-warm vault must not flash a progress line.
    #[test]
    fn a_ready_owner_draws_no_warming_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("already-warm.sock");
        let handle = fake_owner(socket.clone(), |_| pong(ServingState::Ready));

        let mut session = connected_session(&socket);
        let sink = RecordingSink::default();
        session.set_progress_sink(Box::new(sink.clone()));

        session.wait_until_ready(Duration::from_secs(5)).unwrap();
        assert!(sink.observations().is_empty());
        assert_eq!(sink.finishes(), 0);

        drop(session);
        handle.join().unwrap();
    }

    /// F4: a warm-up that finishes INSIDE one `PROGRESS_HEARTBEAT` must draw
    /// nothing, not one observation. Before the fix the throttle clock seeded
    /// on the FIRST not-Ready pong, so even a warm-up finishing in a few
    /// milliseconds still drew one `warming` line — a flash for a wait that was
    /// never actually slow. Seeding the clock at the wait's own start closes
    /// that: the owner here reports `opening` for well under the heartbeat
    /// floor before flipping to `Ready`.
    #[test]
    fn a_sub_interval_warm_up_draws_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("sub-interval-warm-up.sock");
        // `opening` for 200ms — far short of the one-second heartbeat floor.
        let handle = fake_owner(socket.clone(), |started| {
            if started.elapsed() < Duration::from_millis(200) {
                pong(ServingState::Opening)
            } else {
                pong(ServingState::Ready)
            }
        });

        let mut session = connected_session(&socket);
        let sink = RecordingSink::default();
        session.set_progress_sink(Box::new(sink.clone()));

        let got = session.wait_until_ready(Duration::from_secs(5)).unwrap();
        assert_eq!(got.serving, ServingState::Ready);
        assert!(
            sink.observations().is_empty(),
            "a warm-up finishing inside one heartbeat interval must draw nothing: {:?}",
            sink.observations()
        );
        assert_eq!(
            sink.finishes(),
            0,
            "nothing was drawn, so there is nothing to close out"
        );

        drop(session);
        handle.join().unwrap();
    }

    /// The stall verdict does not stop the owner: its answer to the abandoned
    /// request can still land on the socket. A session that reused that socket
    /// would serve those late frames as the NEXT request's answer — a wrong
    /// report, at a clean exit code. The verdict therefore POISONS the session,
    /// and the next request reconnects (here: fails to, since this test session
    /// retains no config) instead of reading the stale frame.
    #[test]
    fn a_stall_verdict_poisons_the_session_against_the_late_answer() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("late-answer.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            let mut line = String::new();
            // Read the first request, answer it LATE (past the client's budget),
            // then keep serving a distinguishable second answer.
            let _ = reader.read_line(&mut line);
            std::thread::sleep(Duration::from_millis(150));
            let mut buf = serde_json::to_vec(&OwnerFrame::Probe {
                document_count: 111,
            })
            .unwrap();
            buf.push(b'\n');
            let _ = writer.write_all(&buf);
            let _ = writer.flush();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let mut buf = serde_json::to_vec(&OwnerFrame::Probe {
                    document_count: 222,
                })
                .unwrap();
                buf.push(b'\n');
                if writer.write_all(&buf).is_err() || writer.flush().is_err() {
                    break;
                }
            }
        });

        let mut session = connected_session(&socket);
        session.set_stall_budget(Duration::from_millis(30));
        let err = session.probe().expect_err("a late answer is a stall");
        assert!(matches!(err, ClientError::OwnerHealth(_)), "got {err:?}");

        // The late `111` is now sitting on the socket. The next request must not
        // return it; with no retained config the forced reconnect cannot
        // succeed, so the caller gets an error rather than a wrong answer.
        match session.probe() {
            Ok(count) => panic!("a poisoned session served the late answer: {count}"),
            Err(e) => assert!(
                matches!(e, ClientError::OwnerUnavailable { .. }),
                "expected the forced reconnect to surface, got {e:?}"
            ),
        }

        drop(session);
        handle.join().unwrap();
    }

    /// A terminal frame of the WRONG kind — here, an owner that answers a
    /// `probe` with a `pong` — leaves the reader one frame behind where the
    /// owner's writer thinks it is: the same desync class a stall verdict
    /// leaves behind. This must poison the session too, so the next request
    /// reconnects rather than reading whatever the (misbehaving) owner sends
    /// next as if it were this request's own answer.
    #[test]
    fn an_unexpected_frame_poisons_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("wrong-kind.sock");
        // Answers every request with a `pong` — wrong for `probe`.
        let handle = fake_owner(socket.clone(), |_| pong(ServingState::Ready));

        let mut session = connected_session(&socket);
        let err = session.probe().expect_err("a pong is not a probe report");
        assert!(matches!(err, ClientError::Protocol(_)), "got {err:?}");

        // With no retained config the forced reconnect cannot succeed; the
        // caller must get that error, never a read off the stale stream.
        match session.probe() {
            Ok(count) => panic!("a poisoned session served a stale-stream read: {count}"),
            Err(e) => assert!(
                matches!(e, ClientError::OwnerUnavailable { .. }),
                "expected the forced reconnect to surface, got {e:?}"
            ),
        }

        drop(session);
        handle.join().unwrap();
    }

    /// A request that emitted no progress must not call `finished` — a sink
    /// drawing a transient line has nothing to erase, so an unconditional
    /// finish would make every fast request flicker.
    #[test]
    fn a_progress_free_request_never_finishes_the_sink() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("instant.sock");
        let handle = fake_owner(socket.clone(), |_| OwnerFrame::Probe { document_count: 7 });

        let mut session = connected_session(&socket);
        let sink = RecordingSink::default();
        session.set_progress_sink(Box::new(sink.clone()));
        assert_eq!(session.probe().unwrap(), 7);
        assert!(sink.observations().is_empty());
        assert_eq!(sink.finishes(), 0);

        drop(session);
        handle.join().unwrap();
    }
}
