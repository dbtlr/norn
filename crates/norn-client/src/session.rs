//! Connecting to a summoned owner and speaking the norn-wire control plane.
//!
//! Synchronous: a CLI invocation is one short-lived process. There is NO
//! in-process cache open anywhere here.
//!
//! # One frame loop, one silence budget
//!
//! A request is one write followed by [`request`](OwnerSession::request)'s frame
//! loop: every [`OwnerFrame::Progress`] is handed to the session's
//! [`ProgressSink`] and the loop reads on; the single terminal frame ends the
//! request. [`STALL_BUDGET`] is the INTER-FRAME silence budget — it is the
//! socket's per-read deadline, so any frame restarts it. A long mutation that
//! heartbeats therefore never trips it, while a wedged owner that emits nothing
//! surfaces as [`ClientError::OwnerHealth`] at the budget (ADR 0013's
//! 2026-07-17 amendment — never a Direct fallback).
//!
//! There is exactly ONE frame loop: waiting for a warming owner reads the same
//! frames through the same loop as any other request.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use norn_wire::{
    ApplyParams, ApplyReport, AuditParams, AuditReport, ClientFrame, CountParams, CountReport,
    DeleteParams, DescribeParams, DescribeReport, EditParams, EditReport, FindParams, FindReport,
    GetParams, GetReport, MoveParams, NewParams, NewReport, OwnerFrame, Progress, RepairParams,
    RepairReport, RewriteWikilinkParams, ServingState, SetParams, SetReport, ValidateParams,
    ValidateReport, WriterProgress, CONTROL_PROTOCOL,
};

use crate::error::ClientError;
use crate::SummonConfig;

/// The service stall budget (ADR 0013): the maximum SILENCE the client tolerates
/// between two frames of one request. Not a call timeout — a healthy owner
/// heartbeats (`norn_wire::PROGRESS_HEARTBEAT`) while its long work runs, and
/// every frame restarts the budget, so only an owner emitting nothing at all is
/// "hung".
pub const STALL_BUDGET: Duration = Duration::from_secs(5);

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
}

/// The parsed proof-of-life a `ping` returns.
#[derive(Debug, Clone)]
pub struct Pong {
    pub version: String,
    pub build: Option<String>,
    pub pid: u32,
    pub serving: ServingState,
    pub writer_progress: WriterProgress,
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
    /// Ready. Requires a retained config (production sessions always have one).
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
                writer_progress,
                ..
            } => Ok(Pong {
                version,
                build,
                pid,
                serving,
                writer_progress,
            }),
            other => Err(unexpected_frame(other, "pong")),
        }
    }

    /// The trivial routed read: the vault's document count (NRN-345 stand-in).
    pub fn probe(&mut self) -> Result<u64, ClientError> {
        match self.request(&ClientFrame::Probe)? {
            OwnerFrame::Probe { document_count } => Ok(document_count),
            other => Err(unexpected_frame(other, "probe report")),
        }
    }

    /// Run a `find` request against the owner's warm cache.
    pub fn find(&mut self, params: FindParams) -> Result<FindReport, ClientError> {
        match self.request(&ClientFrame::Find { params })? {
            OwnerFrame::Find { report } => Ok(report),
            other => Err(unexpected_frame(other, "find report")),
        }
    }

    /// Run a `count` request against the owner's warm cache.
    pub fn count(&mut self, params: CountParams) -> Result<CountReport, ClientError> {
        match self.request(&ClientFrame::Count { params })? {
            OwnerFrame::Count { report } => Ok(report),
            other => Err(unexpected_frame(other, "count report")),
        }
    }

    /// Run a `get` request against the owner's warm cache.
    pub fn get(&mut self, params: GetParams) -> Result<GetReport, ClientError> {
        match self.request(&ClientFrame::Get { params })? {
            OwnerFrame::Get { report } => Ok(report),
            other => Err(unexpected_frame(other, "get report")),
        }
    }

    /// Run a `describe` request against the owner's warm cache + retained config.
    pub fn describe(&mut self, params: DescribeParams) -> Result<DescribeReport, ClientError> {
        match self.request(&ClientFrame::Describe { params })? {
            OwnerFrame::Describe { report } => Ok(report),
            other => Err(unexpected_frame(other, "describe report")),
        }
    }

    /// Run a `validate` request against the owner's warm graph + retained config.
    pub fn validate(&mut self, params: ValidateParams) -> Result<ValidateReport, ClientError> {
        match self.request(&ClientFrame::Validate { params })? {
            OwnerFrame::Validate { report } => Ok(report),
            other => Err(unexpected_frame(other, "validate report")),
        }
    }

    /// Run a `repair` request against the owner's warm graph + retained config.
    /// Read-only: the owner builds the findings-derived `MigrationPlan` and never
    /// writes (the returned plan is the output; `apply` executes it).
    pub fn repair(&mut self, params: RepairParams) -> Result<RepairReport, ClientError> {
        match self.request(&ClientFrame::Repair { params })? {
            OwnerFrame::Repair { report } => Ok(report),
            other => Err(unexpected_frame(other, "repair report")),
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
            other => Err(unexpected_frame(other, "audit report")),
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
            other => Err(unexpected_frame(other, "set report")),
        }
    }

    /// Run a `new` mutation against the owner's warm cache. Same send-once,
    /// never-retry contract as [`set`](Self::set).
    pub fn new_document(&mut self, params: NewParams) -> Result<NewReport, ClientError> {
        match self.request(&ClientFrame::New { params })? {
            OwnerFrame::New { report } => Ok(report),
            other => Err(unexpected_frame(other, "new report")),
        }
    }

    /// Run an `edit` mutation against the owner's warm cache. Same send-once,
    /// never-retry contract as [`set`](Self::set).
    pub fn edit(&mut self, params: EditParams) -> Result<EditReport, ClientError> {
        match self.request(&ClientFrame::Edit { params })? {
            OwnerFrame::Edit { report } => Ok(report),
            other => Err(unexpected_frame(other, "edit report")),
        }
    }

    /// Run a `move` mutation against the owner's warm cache. Same send-once,
    /// never-retry contract as [`set`](Self::set). The report is the shared typed
    /// [`ApplyReport`] (which lives in `norn-wire`), consumed directly by the CLI.
    pub fn move_document(&mut self, params: MoveParams) -> Result<ApplyReport, ClientError> {
        match self.request(&ClientFrame::Move { params })? {
            OwnerFrame::Move { report } => Ok(report),
            other => Err(unexpected_frame(other, "move report")),
        }
    }

    /// Run a `delete` mutation against the owner's warm cache. Same contract as
    /// [`move_document`](Self::move_document).
    pub fn delete(&mut self, params: DeleteParams) -> Result<ApplyReport, ClientError> {
        match self.request(&ClientFrame::Delete { params })? {
            OwnerFrame::Delete { report } => Ok(report),
            other => Err(unexpected_frame(other, "delete report")),
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
            other => Err(unexpected_frame(other, "rewrite-wikilink report")),
        }
    }

    /// Run an `apply` mutation — execute an already-reviewed `MigrationPlan`
    /// (carried typed in `params.plan`) — against the owner's warm cache. Same
    /// send-once, never-retry contract as [`move_document`](Self::move_document);
    /// the report is the shared typed [`ApplyReport`] the CLI consumes directly.
    pub fn apply(&mut self, params: ApplyParams) -> Result<ApplyReport, ClientError> {
        match self.request(&ClientFrame::Apply { params })? {
            OwnerFrame::Apply { report } => Ok(report),
            other => Err(unexpected_frame(other, "apply report")),
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
    /// [`WriterProgress`] rides the pong as a control-plane fact, but NO health
    /// verdict is derived from it: an owner reports progress by emitting frames,
    /// not by advancing a counter a poller inspects.
    ///
    /// Before Ready is first observed, an owner that goes away at the connection
    /// level ([`ClientError::OwnerGone`]) — the linux drain-window backlog race
    /// (see [`crate::open`]) — is self-healed by re-summoning (bounded by
    /// `max_wait`), never surfaced as raw IO. After Ready would be observed the
    /// method has returned, so a mid-request drop on the returned session stays a
    /// hard error (post-send uncertainty is a separate contract).
    pub fn wait_until_ready(&mut self, max_wait: Duration) -> Result<Pong, ClientError> {
        let start = Instant::now();
        loop {
            let pong = match self.ping() {
                Ok(pong) => pong,
                // Owner went away before Ready — resummon and retry, bounded by
                // `max_wait`. A hung owner (OwnerHealth) or other error is NOT
                // healable this way, so it surfaces.
                Err(e) if e.is_owner_gone() => {
                    if start.elapsed() > max_wait {
                        return Err(e);
                    }
                    self.reconnect()?;
                    continue;
                }
                Err(e) => return Err(e),
            };
            if pong.serving == ServingState::Ready {
                return Ok(pong);
            }
            if start.elapsed() > max_wait {
                return Err(ClientError::OwnerHealth(
                    "timed out waiting for the owner to become ready".to_string(),
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Send one frame and read the request's stream to its terminal frame.
    ///
    /// The ONE frame loop (NRN-512). Every [`OwnerFrame::Progress`] is handed to
    /// the progress sink and the loop reads again; the terminal frame returns.
    /// The socket's read deadline is [`stall_budget`](Self::stall_budget), and a
    /// read deadline restarts per `read` call — which is precisely what makes
    /// the budget INTER-FRAME rather than a whole-call timeout. A mutation that
    /// runs for a minute but heartbeats every second is healthy; an owner that
    /// says nothing for a whole budget is hung.
    ///
    /// Post-send failure shapes are unchanged (ADR 0011): EOF mid-stream is
    /// [`ClientError::OwnerGone`] — the request WAS written, so a mutation may
    /// have applied and no caller may blind-retry it — and a silence timeout is
    /// [`ClientError::OwnerHealth`].
    fn request(&mut self, frame: &ClientFrame) -> Result<OwnerFrame, ClientError> {
        let mut line = serde_json::to_vec(frame)
            .map_err(|e| ClientError::Protocol(format!("failed to encode frame: {e}")))?;
        line.push(b'\n');
        // A write to a peer that already went away fails at the connection level
        // (BrokenPipe/ConnectionReset). This is the PRE-SEND shape: the frame was
        // never delivered, so it is safe to resummon and retry (even a mutation).
        // A held owner that idle-reaped between calls fails HERE on the first
        // write — the recovery seam a long-lived session (MCP) heals from.
        self.writer.write_all(&line).map_err(classify_io_pre_send)?;
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
                Ok(OwnerFrame::Progress { progress }) => {
                    observed_progress = true;
                    self.progress.progress(&progress);
                }
                other => {
                    if observed_progress {
                        self.progress.finished();
                    }
                    return other;
                }
            }
        }
    }
}

/// Map a reply that is NOT a verb's own success frame onto a client error — the
/// one shared tail every verb method routes its non-success arm through
/// (NRN-411). A warm-up/user [`OwnerFrame::Rejected`] rides the user-error path
/// ([`ClientError::Rejected`], carrying the message + hints); an
/// [`OwnerFrame::Error`] becomes an [`ClientError::OwnerError`]; any other frame
/// is a protocol mismatch labelled with `expected` (e.g. `"find report"`). A free
/// fn, not a `request`-wrapping closure, so no verb closure returns the large
/// `OwnerFrame` in an `Err` (which `clippy::result_large_err` would flag).
fn unexpected_frame(frame: OwnerFrame, expected: &str) -> ClientError {
    match frame {
        OwnerFrame::Rejected { message, hints } => ClientError::Rejected { message, hints },
        OwnerFrame::Error { message } => ClientError::OwnerError(message),
        other => ClientError::Protocol(format!("expected {expected}, got {other:?}")),
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

    fn pong(serving: ServingState, busy: bool, sequence: u64) -> OwnerFrame {
        OwnerFrame::Pong {
            protocol: CONTROL_PROTOCOL,
            version: "0.0.0".into(),
            build: None,
            pid: 1,
            serving,
            writer_progress: WriterProgress { busy, sequence },
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

    /// Finding 1: a warm-up (non-busy `opening`) that runs LONGER than the stall
    /// budget must still reach `ready` — it is healthy liveness, not a hang.
    #[test]
    fn warmup_longer_than_stall_budget_still_reaches_ready() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("warmup.sock");
        // Non-busy `opening` for 150ms (>> the 50ms budget below), then ready.
        let handle = fake_owner(socket.clone(), |started| {
            if started.elapsed() < Duration::from_millis(150) {
                pong(ServingState::Opening, false, 0)
            } else {
                pong(ServingState::Ready, false, 0)
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

    /// A busy writer stays healthy as long as it keeps answering — the health
    /// verdict is keyed on FRAMES, not on the pong's progress sequence (NRN-512
    /// replaced the sequence-advancement heuristic with the inter-frame silence
    /// budget). A frozen sequence with prompt pongs is not a stall.
    #[test]
    fn busy_writer_with_a_frozen_sequence_still_reaches_ready() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("frozen-seq.sock");
        // Busy, sequence pinned at 7 the whole time — well past the 50ms budget
        // below — then ready. Under the old sequence-stall rule this was an
        // owner-health error; under the frame protocol the prompt pongs ARE the
        // proof of life.
        let handle = fake_owner(socket.clone(), |started| {
            if started.elapsed() < Duration::from_millis(150) {
                pong(ServingState::Opening, true, 7)
            } else {
                pong(ServingState::Ready, true, 7)
            }
        });

        let mut session = connected_session(&socket);
        session.set_stall_budget(Duration::from_millis(50));
        let got = session
            .wait_until_ready(Duration::from_secs(5))
            .expect("an owner that keeps answering is alive, frozen sequence or not");
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
            let mut write = |frame: &OwnerFrame, w: &mut UnixStream| -> bool {
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
    /// (NRN-512's one-emitter rule): a request landing on a warming owner is
    /// answered with `warming` progress frames and then its terminal frame,
    /// through the one frame loop — no pre-Ready special path.
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
            for done in 1..=3u64 {
                std::thread::sleep(Duration::from_millis(30));
                let frame = OwnerFrame::Progress {
                    progress: Progress::new(ProgressPhase::Warming).with_done(done * 100),
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
        assert_eq!(observed[2].done, Some(300), "milestones ride the frame");

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
