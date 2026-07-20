//! Connecting to a summoned owner and speaking the norn-wire control plane.
//!
//! Synchronous: a CLI invocation is one short-lived process. The socket carries
//! a per-request read timeout equal to the stall budget so a hung owner surfaces
//! as an [`ClientError::OwnerHealth`] (ADR 0013's 2026-07-17 amendment — never a
//! Direct fallback). There is NO in-process cache open anywhere here.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use norn_wire::{
    ClientFrame, CountParams, CountReport, DeleteParams, DescribeParams, DescribeReport,
    EditParams, EditReport, FindParams, FindReport, GetParams, GetReport, MoveParams, NewParams,
    NewReport, OwnerFrame, RepairParams, RepairReport, RewriteWikilinkParams, ServingState,
    SetParams, SetReport, ValidateParams, ValidateReport, WriterProgress, CONTROL_PROTOCOL,
};

use crate::error::ClientError;
use crate::SummonConfig;

/// The service stall budget (ADR 0013): the read deadline for a control-plane
/// exchange. Not a call timeout — a healthy busy writer answers pings instantly
/// while its long work runs; only silence past this budget is "hung".
pub const STALL_BUDGET: Duration = Duration::from_secs(5);

/// A live, proven connection to a summoned owner.
pub struct OwnerSession {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    socket: PathBuf,
    /// The sequence-stall budget applied to a BUSY writer (ADR 0013). A field
    /// (not the [`STALL_BUDGET`] const) so tests can shrink it to drive the
    /// busy-stall path fast; production keeps the default.
    stall_budget: Duration,
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
            config,
        })
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
        Ok(())
    }

    /// Test-only: shrink the busy-writer sequence-stall budget so the busy-stall
    /// path is drivable without a multi-second wait.
    #[cfg(test)]
    pub(crate) fn set_stall_budget(&mut self, budget: Duration) {
        self.stall_budget = budget;
    }

    /// The socket this session is bound to.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Liveness + serving-state probe.
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
            // A warm-up that failed on an invalid `.norn/config.yaml` answers
            // every frame — including a ping — with the config error as a
            // Rejected (NRN-360). The owner is healthy (not exit-to-heal), so
            // this rides the user-error path: `wait_until_ready` returns it and
            // the CLI renders the config error, never a resummon/crash loop.
            OwnerFrame::Rejected { message, hints } => {
                Err(ClientError::Rejected { message, hints })
            }
            OwnerFrame::Error { message } => Err(ClientError::OwnerError(message)),
            other => Err(ClientError::Protocol(format!(
                "expected pong, got {other:?}"
            ))),
        }
    }

    /// The trivial routed read: the vault's document count (NRN-345 stand-in).
    pub fn probe(&mut self) -> Result<u64, ClientError> {
        match self.request(&ClientFrame::Probe)? {
            OwnerFrame::Probe { document_count } => Ok(document_count),
            // A bad-config warm-up rejects every frame with the config error
            // (NRN-360) — surface it on the user-error path, like the read verbs.
            OwnerFrame::Rejected { message, hints } => {
                Err(ClientError::Rejected { message, hints })
            }
            OwnerFrame::Error { message } => Err(ClientError::OwnerError(message)),
            other => Err(ClientError::Protocol(format!(
                "expected probe report, got {other:?}"
            ))),
        }
    }

    /// Run a `find` request against the owner's warm cache.
    pub fn find(&mut self, params: FindParams) -> Result<FindReport, ClientError> {
        match self.request(&ClientFrame::Find { params })? {
            OwnerFrame::Find { report } => Ok(report),
            OwnerFrame::Rejected { message, hints } => {
                Err(ClientError::Rejected { message, hints })
            }
            OwnerFrame::Error { message } => Err(ClientError::OwnerError(message)),
            other => Err(ClientError::Protocol(format!(
                "expected find report, got {other:?}"
            ))),
        }
    }

    /// Run a `count` request against the owner's warm cache.
    pub fn count(&mut self, params: CountParams) -> Result<CountReport, ClientError> {
        match self.request(&ClientFrame::Count { params })? {
            OwnerFrame::Count { report } => Ok(report),
            OwnerFrame::Rejected { message, hints } => {
                Err(ClientError::Rejected { message, hints })
            }
            OwnerFrame::Error { message } => Err(ClientError::OwnerError(message)),
            other => Err(ClientError::Protocol(format!(
                "expected count report, got {other:?}"
            ))),
        }
    }

    /// Run a `get` request against the owner's warm cache.
    pub fn get(&mut self, params: GetParams) -> Result<GetReport, ClientError> {
        match self.request(&ClientFrame::Get { params })? {
            OwnerFrame::Get { report } => Ok(report),
            OwnerFrame::Rejected { message, hints } => {
                Err(ClientError::Rejected { message, hints })
            }
            OwnerFrame::Error { message } => Err(ClientError::OwnerError(message)),
            other => Err(ClientError::Protocol(format!(
                "expected get report, got {other:?}"
            ))),
        }
    }

    /// Run a `describe` request against the owner's warm cache + retained config.
    pub fn describe(&mut self, params: DescribeParams) -> Result<DescribeReport, ClientError> {
        match self.request(&ClientFrame::Describe { params })? {
            OwnerFrame::Describe { report } => Ok(report),
            OwnerFrame::Rejected { message, hints } => {
                Err(ClientError::Rejected { message, hints })
            }
            OwnerFrame::Error { message } => Err(ClientError::OwnerError(message)),
            other => Err(ClientError::Protocol(format!(
                "expected describe report, got {other:?}"
            ))),
        }
    }

    /// Run a `validate` request against the owner's warm graph + retained config.
    pub fn validate(&mut self, params: ValidateParams) -> Result<ValidateReport, ClientError> {
        match self.request(&ClientFrame::Validate { params })? {
            OwnerFrame::Validate { report } => Ok(report),
            OwnerFrame::Rejected { message, hints } => {
                Err(ClientError::Rejected { message, hints })
            }
            OwnerFrame::Error { message } => Err(ClientError::OwnerError(message)),
            other => Err(ClientError::Protocol(format!(
                "expected validate report, got {other:?}"
            ))),
        }
    }

    /// Run a `repair` request against the owner's warm graph + retained config.
    /// Read-only: the owner builds the findings-derived `MigrationPlan` and never
    /// writes (the returned plan is the output; `apply` executes it).
    pub fn repair(&mut self, params: RepairParams) -> Result<RepairReport, ClientError> {
        match self.request(&ClientFrame::Repair { params })? {
            OwnerFrame::Repair { report } => Ok(report),
            OwnerFrame::Rejected { message, hints } => {
                Err(ClientError::Rejected { message, hints })
            }
            OwnerFrame::Error { message } => Err(ClientError::OwnerError(message)),
            other => Err(ClientError::Protocol(format!(
                "expected repair report, got {other:?}"
            ))),
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
            OwnerFrame::Rejected { message, hints } => {
                Err(ClientError::Rejected { message, hints })
            }
            OwnerFrame::Error { message } => Err(ClientError::OwnerError(message)),
            other => Err(ClientError::Protocol(format!(
                "expected set report, got {other:?}"
            ))),
        }
    }

    /// Run a `new` mutation against the owner's warm cache. Same send-once,
    /// never-retry contract as [`set`](Self::set).
    pub fn new_document(&mut self, params: NewParams) -> Result<NewReport, ClientError> {
        match self.request(&ClientFrame::New { params })? {
            OwnerFrame::New { report } => Ok(report),
            OwnerFrame::Rejected { message, hints } => {
                Err(ClientError::Rejected { message, hints })
            }
            OwnerFrame::Error { message } => Err(ClientError::OwnerError(message)),
            other => Err(ClientError::Protocol(format!(
                "expected new report, got {other:?}"
            ))),
        }
    }

    /// Run an `edit` mutation against the owner's warm cache. Same send-once,
    /// never-retry contract as [`set`](Self::set).
    pub fn edit(&mut self, params: EditParams) -> Result<EditReport, ClientError> {
        match self.request(&ClientFrame::Edit { params })? {
            OwnerFrame::Edit { report } => Ok(report),
            OwnerFrame::Rejected { message, hints } => {
                Err(ClientError::Rejected { message, hints })
            }
            OwnerFrame::Error { message } => Err(ClientError::OwnerError(message)),
            other => Err(ClientError::Protocol(format!(
                "expected edit report, got {other:?}"
            ))),
        }
    }

    /// Run a `move` mutation against the owner's warm cache. Same send-once,
    /// never-retry contract as [`set`](Self::set). The report is the shared
    /// `ApplyReport` as an opaque JSON value (the wire cannot name the core type —
    /// see `norn_wire`); the CLI, which links norn-core, deserializes it.
    pub fn move_document(&mut self, params: MoveParams) -> Result<serde_json::Value, ClientError> {
        match self.request(&ClientFrame::Move { params })? {
            OwnerFrame::Move { report } => Ok(report),
            OwnerFrame::Rejected { message, hints } => {
                Err(ClientError::Rejected { message, hints })
            }
            OwnerFrame::Error { message } => Err(ClientError::OwnerError(message)),
            other => Err(ClientError::Protocol(format!(
                "expected move report, got {other:?}"
            ))),
        }
    }

    /// Run a `delete` mutation against the owner's warm cache. Same contract as
    /// [`move_document`](Self::move_document).
    pub fn delete(&mut self, params: DeleteParams) -> Result<serde_json::Value, ClientError> {
        match self.request(&ClientFrame::Delete { params })? {
            OwnerFrame::Delete { report } => Ok(report),
            OwnerFrame::Rejected { message, hints } => {
                Err(ClientError::Rejected { message, hints })
            }
            OwnerFrame::Error { message } => Err(ClientError::OwnerError(message)),
            other => Err(ClientError::Protocol(format!(
                "expected delete report, got {other:?}"
            ))),
        }
    }

    /// Run a `rewrite-wikilink` mutation against the owner's warm cache. Same
    /// contract as [`move_document`](Self::move_document).
    pub fn rewrite_wikilink(
        &mut self,
        params: RewriteWikilinkParams,
    ) -> Result<serde_json::Value, ClientError> {
        match self.request(&ClientFrame::RewriteWikilink { params })? {
            OwnerFrame::RewriteWikilink { report } => Ok(report),
            OwnerFrame::Rejected { message, hints } => {
                Err(ClientError::Rejected { message, hints })
            }
            OwnerFrame::Error { message } => Err(ClientError::OwnerError(message)),
            other => Err(ClientError::Protocol(format!(
                "expected rewrite-wikilink report, got {other:?}"
            ))),
        }
    }

    /// Ping until the owner reports [`ServingState::Ready`], honoring ADR 0013's
    /// liveness contract and 0017's accepted warm-up cost:
    ///
    /// - **A prompt pong is proof of life.** A truly hung owner (no pong at all)
    ///   is caught by the socket read timeout inside [`ping`](Self::ping), which
    ///   surfaces as [`ClientError::OwnerHealth`]. So as long as pongs keep
    ///   arriving, the owner is alive.
    /// - **Warm-up is healthy, however long it takes.** A `cold`/`opening` owner
    ///   that is NOT busy is running the one-shot full build — warm-up is ~linear
    ///   in vault size (0017's accepted cost) and, crucially, happens BEFORE the
    ///   writer queue exists, so `writer_progress` is default the whole time.
    ///   Keying a stall on sequence advancement here would mis-declare every
    ///   warm-up longer than the budget as hung. We do NOT: a non-busy warming
    ///   owner is waited on indefinitely (bounded only by `max_wait`).
    /// - **Only a busy writer whose sequence stalls is hung** (0013). The
    ///   sequence-stall budget applies solely while `busy == true`.
    ///
    /// Before Ready is first observed, an owner that goes away at the connection
    /// level ([`ClientError::OwnerGone`]) — the linux drain-window backlog race
    /// (see [`crate::open`]) — is self-healed by re-summoning (bounded by
    /// `max_wait`), never surfaced as raw IO. After Ready would be observed the
    /// method has returned, so a mid-request drop on the returned session stays a
    /// hard error (post-send uncertainty is a separate contract).
    pub fn wait_until_ready(&mut self, max_wait: Duration) -> Result<Pong, ClientError> {
        let start = Instant::now();
        let mut last_seq: Option<u64> = None;
        let mut busy_since: Option<Instant> = None;
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
                    last_seq = None;
                    busy_since = None;
                    continue;
                }
                Err(e) => return Err(e),
            };
            if pong.serving == ServingState::Ready {
                return Ok(pong);
            }
            if pong.writer_progress.busy {
                // A busy writer must keep advancing its sequence; a stall past
                // the budget is the one "hung" signal (ADR 0013).
                let seq = pong.writer_progress.sequence;
                if Some(seq) != last_seq {
                    last_seq = Some(seq);
                    busy_since = Some(Instant::now());
                }
                if busy_since.is_some_and(|t| t.elapsed() > self.stall_budget) {
                    return Err(ClientError::OwnerHealth(
                        "owner writer busy but its progress sequence stalled past the budget"
                            .to_string(),
                    ));
                }
            } else {
                // Not ready, not busy: warm-up in flight (the writer queue does
                // not exist yet). The prompt pong is liveness; reset the busy
                // stall tracking and keep waiting.
                last_seq = None;
                busy_since = None;
            }
            if start.elapsed() > max_wait {
                return Err(ClientError::OwnerHealth(
                    "timed out waiting for the owner to become ready".to_string(),
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn request(&mut self, frame: &ClientFrame) -> Result<OwnerFrame, ClientError> {
        let mut line = serde_json::to_vec(frame)
            .map_err(|e| ClientError::Protocol(format!("failed to encode frame: {e}")))?;
        line.push(b'\n');
        // A write to a peer that already went away fails at the connection level
        // (BrokenPipe/ConnectionReset) — classify as OwnerGone, not raw IO.
        self.writer.write_all(&line).map_err(classify_io)?;
        self.writer.flush().map_err(classify_io)?;

        let mut resp = String::new();
        match self.reader.read_line(&mut resp) {
            // EOF before a reply == the owner exited mid-exchange (the drain-window
            // shape) — a resummon signal, not a hang.
            Ok(0) => Err(ClientError::OwnerGone(
                "owner closed the connection before replying".to_string(),
            )),
            Ok(_) => serde_json::from_str(resp.trim())
                .map_err(|e| ClientError::Protocol(format!("undecodable owner frame: {e}"))),
            Err(e) if is_timeout(&e) => Err(ClientError::OwnerHealth(
                "no reply from owner within the stall budget".to_string(),
            )),
            Err(e) => Err(classify_io(e)),
        }
    }
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// Map a socket IO error to [`ClientError::OwnerGone`] when it is a
/// connection-level drop (the owner went away — the resummon signal), else to a
/// raw [`ClientError::Io`].
fn classify_io(e: std::io::Error) -> ClientError {
    use std::io::ErrorKind::*;
    if matches!(
        e.kind(),
        BrokenPipe | ConnectionReset | ConnectionAborted | UnexpectedEof | NotConnected
    ) {
        ClientError::OwnerGone(e.to_string())
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
                    "expected the oracle-shaped config message, got {message:?}"
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

    /// A BUSY writer whose sequence never advances IS hung past the budget.
    #[test]
    fn busy_writer_with_stalled_sequence_is_owner_health() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("stalled.sock");
        let handle = fake_owner(socket.clone(), |_| pong(ServingState::Opening, true, 7));

        let mut session = connected_session(&socket);
        session.set_stall_budget(Duration::from_millis(50));
        let err = session
            .wait_until_ready(Duration::from_secs(5))
            .expect_err("a busy writer with a stalled sequence must be owner-health");
        assert!(matches!(err, ClientError::OwnerHealth(_)), "got {err:?}");

        drop(session);
        handle.join().unwrap();
    }

    /// A busy writer that keeps ADVANCING its sequence is healthy and reaches
    /// ready even across many budget windows.
    #[test]
    fn busy_writer_advancing_sequence_reaches_ready() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("advancing.sock");
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let c = std::sync::Arc::clone(&counter);
        let handle = fake_owner(socket.clone(), move |started| {
            let seq = c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if started.elapsed() < Duration::from_millis(150) {
                pong(ServingState::Opening, true, seq) // busy but advancing
            } else {
                pong(ServingState::Ready, false, seq)
            }
        });

        let mut session = connected_session(&socket);
        session.set_stall_budget(Duration::from_millis(50));
        let got = session.wait_until_ready(Duration::from_secs(5)).unwrap();
        assert_eq!(got.serving, ServingState::Ready);

        drop(session);
        handle.join().unwrap();
    }
}
