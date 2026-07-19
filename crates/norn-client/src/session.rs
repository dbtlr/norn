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
        })
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
            other => Err(ClientError::Protocol(format!("expected pong, got {other:?}"))),
        }
    }

    /// The trivial routed read: the vault's document count (NRN-345 stand-in).
    pub fn probe(&mut self) -> Result<u64, ClientError> {
        match self.request(&ClientFrame::Probe)? {
            OwnerFrame::Probe { document_count } => Ok(document_count),
            OwnerFrame::Error { message } => Err(ClientError::OwnerError(message)),
            other => Err(ClientError::Protocol(format!("expected probe report, got {other:?}"))),
        }
    }

    /// Ping until the owner reports [`ServingState::Ready`], honoring ADR 0013:
    /// wait indefinitely (up to `max_wait`) while the owner is healthy — either
    /// answering promptly with `opening`/`cold` and a writer whose `sequence`
    /// keeps advancing (warm-up in flight). Declares [`ClientError::OwnerHealth`]
    /// if the writer's sequence stalls past `STALL_BUDGET` while not ready, or if
    /// `max_wait` elapses.
    pub fn wait_until_ready(&mut self, max_wait: Duration) -> Result<Pong, ClientError> {
        let start = Instant::now();
        let mut last_seq: Option<u64> = None;
        let mut last_progress_at = Instant::now();
        loop {
            let pong = self.ping()?; // a read timeout inside becomes OwnerHealth
            if pong.serving == ServingState::Ready {
                return Ok(pong);
            }
            let seq = pong.writer_progress.sequence;
            if Some(seq) != last_seq {
                last_seq = Some(seq);
                last_progress_at = Instant::now();
            }
            if last_progress_at.elapsed() > STALL_BUDGET {
                return Err(ClientError::OwnerHealth(
                    "owner warm-up stalled (writer sequence not advancing)".to_string(),
                ));
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
pub(crate) fn connect_with_retry(socket: &Path, budget: Duration) -> Result<UnixStream, ClientError> {
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
