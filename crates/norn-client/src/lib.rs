#![deny(unsafe_code)]
//! The summoner + connector (ADR 0017): resolve a vault, derive its build-keyed
//! owner socket, connect to a live owner or summon one, then speak the norn-wire
//! control plane. The only crate that spawns owners or dials sockets client-side.
//!
//! May never: Open caches, or depend on norn-core. (Enforced by the crate map:
//! this crate depends on `norn-wire`, not `norn-core`, so there is no `Cache`
//! type in scope to open.)
//!
//! # Flow (`open`)
//!
//! 1. Resolve the vault root (via `norn-config`'s resolver — name / cwd / env).
//! 2. Derive the socket path: `<runtime_dir>/<h>.<fp>.sock` (see [`addr`]).
//! 3. Try to connect. A live owner answers → done.
//! 4. No owner → spawn one (the current executable in owner mode, detached),
//!    then connect with bounded retry as it binds.
//! 5. Speak norn-wire: `ping` for liveness/serving-state, `probe` for the
//!    trivial routed read. A hung owner surfaces as [`ClientError::OwnerHealth`],
//!    NEVER a Direct fallback (ADR 0013's 2026-07-17 amendment).

mod addr;
mod error;
mod session;
mod summon;

use std::path::PathBuf;
use std::time::{Duration, Instant};

pub use addr::{build_fingerprint, runtime_dir_from_env, socket_path};
pub use error::ClientError;
pub use session::{OwnerSession, Pong, STALL_BUDGET};
pub use summon::OWNER_MODE_ARG;

// Re-export the resolver vocabulary so a caller wires the CLI's `--vault` / `-C`
// straight through without also naming `norn-config`.
pub use norn_config::{ConfigHome, Registry, ResolveInput, Resolved, ResolvedVia};

/// One-line boundary contract, referenced by every dependent so each
/// declared edge in the crate map is compiler-load-bearing.
pub const CONTRACT: &str =
    "norn-client: summoner and connector — the only crate that spawns owners or dials sockets";

/// Direct-dependency contracts — the code reference that makes this
/// crate's declared edges load-bearing rather than manifest-only.
pub const DEP_CONTRACTS: &[&str] = &[norn_wire::CONTRACT, norn_config::CONTRACT];

/// Env override for the ephemeral idle TTL, in seconds. Operational knob (all
/// builds), not test-only.
pub const EPHEMERAL_TTL_ENV: &str = "NORN_EPHEMERAL_TTL_SECS";

/// Default ephemeral owner idle TTL: 120 seconds.
///
/// Rationale (decided, NRN-345): agent workflows issue command bursts with gaps
/// of seconds-to-minutes; 120s covers those burst gaps so re-summon warm-up
/// (~linear in vault size) isn't paid per command, while bounding orphan
/// lifetime to ~2 minutes. The owner-lifetime flock + TTL together make any
/// orphan detectable and self-reaping.
pub const DEFAULT_EPHEMERAL_TTL: Duration = Duration::from_secs(120);

/// The ephemeral idle TTL, honoring [`EPHEMERAL_TTL_ENV`] when it parses to a
/// non-negative integer, else [`DEFAULT_EPHEMERAL_TTL`].
///
/// Env-var semantics (POSIX-by-default, ADR 0020): an *empty* or *unset*
/// variable means "unset" → the default. An *invalid* value (non-numeric,
/// negative, or overflowing `u64`) is a **fail-safe to the default**, never a
/// hard error: this is a resource/performance tuning knob (it bounds only an
/// idle owner's lifetime, never touches vault correctness), and aborting a
/// command because an advanced knob is mistyped would be worse than falling
/// back to the sound default. The fallback is deliberately silent — the knob is
/// off the daily path and the default is always safe.
pub fn ephemeral_idle_ttl() -> Duration {
    match std::env::var(EPHEMERAL_TTL_ENV) {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(secs) => Duration::from_secs(secs),
            Err(_) => DEFAULT_EPHEMERAL_TTL,
        },
        Err(_) => DEFAULT_EPHEMERAL_TTL,
    }
}

/// Default budget for a freshly-spawned owner to bind its socket.
pub const DEFAULT_CONNECT_BUDGET: Duration = Duration::from_secs(10);

/// Everything [`open`] needs, supplied by value so it is testable without
/// touching the real runtime dir or spawning the real bin.
#[derive(Debug, Clone)]
pub struct SummonConfig {
    /// The canonical vault root the owner serves.
    pub vault_root: PathBuf,
    /// The runtime dir base sockets live under (see [`runtime_dir_from_env`]).
    pub runtime_dir: PathBuf,
    /// The build fingerprint (see [`build_fingerprint`]).
    pub fingerprint: String,
    /// Idle TTL handed to a newly-summoned owner.
    pub idle_ttl: Duration,
    /// The executable to spawn in owner mode. Production: `current_exe()`.
    pub owner_exe: PathBuf,
    /// How long to wait for a spawned owner to bind before giving up.
    pub connect_budget: Duration,
    /// The resolver-derived `[vaults.<name>].config` override path (ADR 0017),
    /// when the resolved vault is registered with one. `None` → the owner loads
    /// `<vault_root>/.norn/config.yaml` (the default). Passed to a freshly
    /// summoned owner; an already-live owner keeps the config it warmed under.
    pub config_override: Option<PathBuf>,
}

impl SummonConfig {
    /// Build a config for `vault_root` with the ambient runtime dir, this
    /// build's fingerprint, the env-or-default TTL, and `owner_exe` as the
    /// process to summon (`current_exe()` in production).
    pub fn for_vault(vault_root: PathBuf, owner_exe: PathBuf) -> Result<Self, ClientError> {
        Ok(Self {
            vault_root,
            runtime_dir: runtime_dir_from_env()?,
            fingerprint: build_fingerprint(),
            idle_ttl: ephemeral_idle_ttl(),
            owner_exe,
            connect_budget: DEFAULT_CONNECT_BUDGET,
            config_override: None,
        })
    }

    /// Set the resolver-derived config override the summoned owner warms under.
    pub fn with_config_override(mut self, config_override: Option<PathBuf>) -> Self {
        self.config_override = config_override;
        self
    }
}

/// Summon-or-connect (no handshake): validate the runtime dir, derive the
/// socket, connect to a live owner, or spawn one and connect with bounded retry
/// as it binds. Returns the raw stream + socket; the caller wraps it (peer-uid,
/// handshake). Shared by [`open`] and the session's self-healing reconnect.
pub(crate) fn connect_or_summon(
    config: &SummonConfig,
) -> Result<(std::os::unix::net::UnixStream, PathBuf), ClientError> {
    // Validate (and create 0700) the runtime dir BEFORE any connect. The socket
    // path is computable, so a symlinked or foreign-owned runtime dir must be
    // rejected before we ever dial a socket inside it (security).
    ensure_runtime_dir_0700(&config.runtime_dir)?;
    let socket = socket_path(&config.vault_root, &config.runtime_dir, &config.fingerprint);

    // A live owner already serving this vault+build? Connect and done.
    if let Ok(stream) = session::connect(&socket) {
        return Ok((stream, socket));
    }

    // No owner — summon one (the runtime dir is already validated above).
    summon::spawn_owner(
        &config.owner_exe,
        &socket,
        &config.vault_root,
        config.idle_ttl,
        &config.fingerprint,
        config.config_override.as_deref(),
    )?;

    // Connect with bounded retry as it binds. A losing-flock racer's client
    // still connects here — to whichever owner won and bound the socket.
    let stream = session::connect_with_retry(&socket, config.connect_budget)?;
    Ok((stream, socket))
}

/// Connect to the vault's owner, summoning one if none is live. Never opens a
/// cache in-process; the returned [`OwnerSession`] is the sole access path.
///
/// # The linux drain-window backlog race
///
/// A summoned owner idle-reaps by dropping its listener and exiting. On Linux a
/// client `connect()` that lands after the reaper's listener-drop decision but
/// before the process fully exits still succeeds — into the dying owner's accept
/// backlog — so a bare connect can hand back a socket whose first exchange then
/// fails with BrokenPipe / EOF as the owner exits. (macOS refuses such a connect
/// immediately, which is why this only bit Linux CI.)
///
/// To self-heal invisibly, `open` performs one `ping` round-trip as a
/// connection-verification handshake before returning the session: if that
/// handshake reports the owner went away ([`ClientError::is_owner_gone`]), it
/// loops back into summon-or-connect — bounded by `connect_budget`, with backoff,
/// never spinning. Every reconnect re-runs the peer-uid check.
pub fn open(config: &SummonConfig) -> Result<OwnerSession, ClientError> {
    let deadline = Instant::now() + config.connect_budget;
    let mut backoff = Duration::from_millis(5);
    loop {
        let (stream, socket) = connect_or_summon(config)?;
        // `OwnerSession::new` verifies the peer uid (a squatter yields a security
        // error, never a fall-through to spawn).
        let mut session = OwnerSession::new(stream, socket, Some(config.clone()))?;
        // Handshake: prove the connection is a live owner, not a stale backlog
        // connection into a dying one (see the linux-backlog race above).
        match session.ping() {
            Ok(_) => return Ok(session),
            Err(e) if e.is_owner_gone() && Instant::now() < deadline => {
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_millis(100));
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Resolve a vault through the central registry (name / binding / env / cwd).
/// The tier is always ephemeral in the rewrite (ADR 0017: all dev builds), so an
/// [`ResolvedVia::UnregisteredCwd`] outcome is not an error — its root still gets
/// a summoned ephemeral owner.
pub fn resolve(home: ConfigHome, input: &ResolveInput) -> Result<Resolved, ClientError> {
    Registry::new(home)
        .resolve(input)
        .map_err(ClientError::from)
}

/// Create the runtime dir 0700 and validate it (finding 5). The client is the
/// first to create/adopt the dir and runs as the same uid the owner will, so it
/// is the ownership-authoritative check: after create, `lstat` and REJECT a
/// symlink or a dir owned by another uid — the classic pre-creation vector on
/// the world-writable `$TMPDIR/norn-<uid>` fallback.
fn ensure_runtime_dir_0700(dir: &std::path::Path) -> Result<(), ClientError> {
    std::fs::create_dir_all(dir).map_err(ClientError::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = std::fs::symlink_metadata(dir).map_err(ClientError::Io)?; // lstat, no follow
        if meta.file_type().is_symlink() {
            return Err(ClientError::InsecureRuntimeDir(format!(
                "{} is a symlink",
                dir.display()
            )));
        }
        let me = addr::current_uid();
        if meta.uid() != me {
            return Err(ClientError::InsecureRuntimeDir(format!(
                "{} is owned by uid {} (expected {me})",
                dir.display(),
                meta.uid()
            )));
        }
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(ClientError::Io)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ephemeral_ttl_defaults_to_120s_without_env() {
        // Only meaningful when the env is unset; guard so a caller's env can't
        // flake it.
        if std::env::var(EPHEMERAL_TTL_ENV).is_err() {
            assert_eq!(ephemeral_idle_ttl(), Duration::from_secs(120));
        }
    }

    #[test]
    fn ensure_runtime_dir_creates_owned_0700() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("norn-rt");
        ensure_runtime_dir_0700(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn ensure_runtime_dir_rejects_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = tmp.path().join("norn-rt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = ensure_runtime_dir_0700(&link).expect_err("a symlinked runtime dir is insecure");
        assert!(
            matches!(err, ClientError::InsecureRuntimeDir(_)),
            "got {err:?}"
        );
    }
}
