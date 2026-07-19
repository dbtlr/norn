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

use norn_wire::{ClientFrame, OwnerFrame, ServingState, WriterProgress, CONTROL_PROTOCOL};

use crate::error::ClientError;

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
    /// Wrap a connected stream. Sets the per-request read timeout (stall budget).
    pub(crate) fn new(stream: UnixStream, socket: PathBuf) -> Result<Self, ClientError> {
        stream
            .set_read_timeout(Some(STALL_BUDGET))
            .map_err(ClientError::Io)?;
        let writer = stream.try_clone().map_err(ClientError::Io)?;
        Ok(Self {
            reader: BufReader::new(stream),
            writer,
            socket,
            stall_budget: STALL_BUDGET,
        })
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
            OwnerFrame::Error { message } => Err(ClientError::OwnerError(message)),
            other => Err(ClientError::Protocol(format!(
                "expected probe report, got {other:?}"
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
    pub fn wait_until_ready(&mut self, max_wait: Duration) -> Result<Pong, ClientError> {
        let start = Instant::now();
        let mut last_seq: Option<u64> = None;
        let mut busy_since: Option<Instant> = None;
        loop {
            let pong = self.ping()?; // a read timeout inside becomes OwnerHealth
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
        self.writer.write_all(&line).map_err(ClientError::Io)?;
        self.writer.flush().map_err(ClientError::Io)?;

        let mut resp = String::new();
        match self.reader.read_line(&mut resp) {
            Ok(0) => Err(ClientError::OwnerHealth(
                "owner closed the connection before replying".to_string(),
            )),
            Ok(_) => serde_json::from_str(resp.trim())
                .map_err(|e| ClientError::Protocol(format!("undecodable owner frame: {e}"))),
            Err(e) if is_timeout(&e) => Err(ClientError::OwnerHealth(
                "no reply from owner within the stall budget".to_string(),
            )),
            Err(e) => Err(ClientError::Io(e)),
        }
    }
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Write};

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
        OwnerSession::new(stream, socket.to_path_buf()).unwrap()
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
