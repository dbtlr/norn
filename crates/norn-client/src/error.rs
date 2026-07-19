//! The summoner's error type. No `anyhow` in the public API — operator-quality
//! variants a caller can match on.

use std::path::PathBuf;

/// Why a summon-or-connect failed.
#[derive(Debug)]
pub enum ClientError {
    /// Central-config resolution failed (unknown name, stale entry, …).
    Resolve(norn_config::ConfigError),
    /// The runtime dir for sockets could not be determined (neither
    /// `XDG_RUNTIME_DIR` nor `TMPDIR`/uid usable).
    NoRuntimeDir,
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
    /// An IO error talking to the owner over the socket.
    Io(std::io::Error),
    /// A malformed or unexpected frame from the owner.
    Protocol(String),
    /// The owner answered a request with an error frame (e.g. exit-to-heal).
    OwnerError(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Resolve(e) => write!(f, "vault resolution failed: {e}"),
            ClientError::NoRuntimeDir => write!(
                f,
                "cannot determine a runtime dir for owner sockets (set XDG_RUNTIME_DIR or TMPDIR)"
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
            ClientError::Io(e) => write!(f, "owner socket io error: {e}"),
            ClientError::Protocol(msg) => write!(f, "owner protocol error: {msg}"),
            ClientError::OwnerError(msg) => write!(f, "owner returned an error: {msg}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<norn_config::ConfigError> for ClientError {
    fn from(e: norn_config::ConfigError) -> Self {
        ClientError::Resolve(e)
    }
}
