//! The summoner's error type. No `anyhow` in the public API — operator-quality
//! variants a caller can match on.

use std::path::PathBuf;

/// Why a summon-or-connect failed.
#[derive(Debug)]
pub enum ClientError {
    /// Central-config resolution failed (unknown name, stale entry, …). Boxed
    /// so a `ClientError` stays small (`ConfigError` carries paths + strings).
    Resolve(Box<norn_config::ConfigError>),
    /// The runtime dir for sockets could not be determined (neither
    /// `XDG_RUNTIME_DIR` nor `TMPDIR`/uid usable).
    NoRuntimeDir,
    /// The runtime dir failed its security check — it is a symlink, or it is
    /// owned by another uid (a pre-creation / squatting attempt on the
    /// world-writable `$TMPDIR/norn-<uid>` fallback).
    InsecureRuntimeDir(String),
    /// The process listening at the socket runs as a different uid than us — a
    /// squatter at the computable socket path. We refuse to speak wire to it and
    /// never fall through to spawning over it.
    ForeignOwner { peer_uid: u32, expected_uid: u32 },
    /// Spawning the owner process failed.
    Spawn {
        exe: PathBuf,
        source: std::io::Error,
    },
    /// The owner never became reachable within the summon budget (spawned but
    /// never bound, or died on startup — e.g. lost the flock race AND the winner
    /// also vanished). ADR 0017: no owner means summon; there is no Direct path.
    OwnerUnavailable { socket: PathBuf },
    /// The owner is reachable but unhealthy: no pong within the stall budget, or
    /// a busy writer whose progress sequence stalled past it (ADR 0013's
    /// 2026-07-17 amendment — surfaced as an owner-health event, never a reroute).
    OwnerHealth(String),
    /// The owner went away at the connection level — the socket exchange failed
    /// with a connection-level error (EOF / BrokenPipe / ConnectionReset /
    /// UnexpectedEof). This is the resummon signal: a self-healable "owner gone",
    /// distinct from [`OwnerHealth`](ClientError::OwnerHealth) (reachable but
    /// hung) — see the linux-backlog race documented on `open`.
    OwnerGone(String),
    /// An IO error talking to the owner over the socket.
    Io(std::io::Error),
    /// A malformed or unexpected frame from the owner.
    Protocol(String),
    /// The owner answered a request with an error frame (e.g. exit-to-heal).
    OwnerError(String),
    /// The owner REJECTED a well-formed request for a non-cache reason — a bad
    /// predicate, an unresolvable `--links-to` target. The owner is healthy; the
    /// caller surfaces this as an operational failure (the message is
    /// user-facing), NOT an owner-health event.
    Rejected(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Resolve(e) => write!(f, "vault resolution failed: {e}"),
            ClientError::NoRuntimeDir => write!(
                f,
                "cannot determine a runtime dir for owner sockets (set XDG_RUNTIME_DIR or TMPDIR)"
            ),
            ClientError::InsecureRuntimeDir(msg) => {
                write!(f, "refusing an insecure runtime dir: {msg}")
            }
            ClientError::ForeignOwner {
                peer_uid,
                expected_uid,
            } => write!(
                f,
                "refusing owner socket served by uid {peer_uid} (expected {expected_uid})"
            ),
            ClientError::Spawn { exe, source } => {
                write!(f, "failed to spawn owner {}: {source}", exe.display())
            }
            ClientError::OwnerUnavailable { socket } => write!(
                f,
                "summoned owner never became reachable at {}",
                socket.display()
            ),
            ClientError::OwnerHealth(msg) => write!(f, "owner health: {msg}"),
            ClientError::OwnerGone(msg) => write!(f, "owner went away: {msg}"),
            ClientError::Io(e) => write!(f, "owner socket io error: {e}"),
            ClientError::Protocol(msg) => write!(f, "owner protocol error: {msg}"),
            ClientError::OwnerError(msg) => write!(f, "owner returned an error: {msg}"),
            ClientError::Rejected(msg) => write!(f, "{msg}"),
        }
    }
}

impl ClientError {
    /// Whether this is a self-healable "owner went away" at the connection level
    /// (the resummon signal). `true` only for [`OwnerGone`](ClientError::OwnerGone)
    /// — an [`OwnerHealth`](ClientError::OwnerHealth) hang is NOT healable by a
    /// reconnect (the owner is reachable, just stuck).
    pub fn is_owner_gone(&self) -> bool {
        matches!(self, ClientError::OwnerGone(_))
    }
}

impl std::error::Error for ClientError {}

impl From<norn_config::ConfigError> for ClientError {
    fn from(e: norn_config::ConfigError) -> Self {
        ClientError::Resolve(Box::new(e))
    }
}
