//! The CLI → service routing seam (NRN-92, retargeted to a host daemon in
//! NRN-93).
//!
//! On each invocation the CLI decides, for a routable read command, whether a
//! warm `norn-service` host daemon is live and can serve the request from an
//! already-verified warm cache — or whether to run the operation **directly**
//! with its own integrity-verified cache open (today's behavior).
//!
//! The decision is the trust hinge of the whole `norn-service` design
//! (ADR 0005): trust is *inherited from a live authority or re-established
//! locally, never skipped*. A routed request rides a proven-live handshake; an
//! unrouted request re-verifies itself. There is no stale-lease window because
//! liveness is proven per-request, not stamped.
//!
//! ## The routing contract
//!
//! ```text
//! 1. socket file exists?                 # a cheap filesystem stat
//!      no  -> Direct  (own verified open) # common case, zero added latency
//!      yes -> continue
//! 2. handshake on the control path, short timeout
//!      prompt pong  -> Route(conn)        # a live authority answered
//!      timeout/refused/garbage -> Direct  # hung or dead daemon => fall back
//! ```
//!
//! Two guards keep this honest and cheap: **socket-existence is the no-service
//! fast path** (no daemon => only a `stat`, never a handshake timeout), and the
//! **handshake must ride an O(1) control path** so a busy service still answers
//! the ping immediately (the daemon side of that is NRN-93).
//!
//! ## One host daemon, not one per vault (ADR 0005 amendment, 2026-07-04)
//!
//! The original design (NRN-92) derived a per-vault socket path from the
//! vault root's identity hash, so N open vaults meant N independent
//! single-owner daemons with no central coordinator. That derived path
//! measured ~101 bytes on a real vault under a typical `XDG_CACHE_HOME`,
//! leaving almost no headroom under macOS's ~104-byte `sun_path` limit before
//! silently overflowing into "socket unreachable, fall back to Direct" — and
//! running N daemons multiplies the ops burden (N lifecycles, N crash
//! surfaces, N places a stale process can linger) for no benefit the vault
//! itself needs.
//!
//! The revised design is **one host daemon at a single well-known socket**:
//! [`host_socket_path`] is `<XDG_CACHE_HOME>/norn/run/norn.sock`, fixed
//! regardless of how many vaults are open. There is no per-vault derivation to
//! overflow. The daemon names the vault for a connection itself, via a
//! `hello` preamble frame the client sends after the control handshake (see
//! below) — the socket path no longer encodes vault identity at all.
//!
//! ## Scope of this module (NRN-92 / NRN-93)
//!
//! This lands the *client half*: host-socket-path derivation, the probe, the
//! control-frame protocol, and (NRN-94) the request path —
//! [`ServiceClient::call_tool_structured_phased`] sends the `hello` vault preamble and
//! runs the MCP `initialize` + `tools/call` exchange over the same stream. The
//! daemon that answers is NRN-93. The routing seam in `src/lib.rs` decides which
//! commands actually route (only `count` today — see `try_route_read`); every
//! other read, and every routing failure, falls through to a verified direct
//! open, which always preserves the trust invariant.
//!
//! Unix-domain sockets are Unix-only; on non-Unix targets [`probe`] is a
//! compile-time `Direct`, so the crate still builds everywhere.

use camino::{Utf8Path, Utf8PathBuf};

// The `norn service` launchd supervisor (NRN-115). Co-located with the routing
// client because both are the `norn service`/`norn serve` story and the
// supervisor reuses this module's socket/run-dir derivation. `command` is
// cross-platform (its `run` gates non-macOS hosts at runtime with the friendly
// fallback); the layers it wires — `plist`/`launchd`/`status` — are unix-only
// so a non-unix build carries no unused supervisor code (nothing on that
// target could ever call it, and `-D warnings` would flag every item).
pub mod command;
#[cfg(unix)]
pub mod launchd;
#[cfg(unix)]
pub mod plist;
#[cfg(unix)]
pub mod status;

/// Control-frame protocol version. Bumped only on a breaking change to the
/// handshake wire shape; the client refuses to route unless the daemon echoes
/// the same version, so a version skew falls back to Direct rather than
/// misinterpreting frames.
pub const CONTROL_PROTOCOL: u32 = 1;

/// The control-frame wire protocol: newline-delimited JSON, one frame per
/// line, tagged on `norn_control` so client and daemon can dispatch on the
/// frame kind without guessing from shape.
///
/// A single internally-tagged enum is the wire contract for *both* halves —
/// the client constructs [`ControlFrame::Ping`] and parses
/// [`ControlFrame::Pong`] today; the daemon (a later task) constructs `Pong`
/// and parses `Ping`/`Hello`, and answers a named-vault connection with
/// `Ready` or `Error`. Defining every variant now, even the ones the client
/// doesn't speak yet, pins the wire shape for both sides in one place instead
/// of letting the daemon task invent its own ad hoc framing.
///
/// Wire shapes (exactly, one JSON object per line):
/// - ping:  `{"norn_control":"ping","protocol":1}`
/// - pong:  `{"norn_control":"pong","protocol":1,"version":"<semver>","pid":<u32>,"uptime_secs":<u64>}`
/// - hello: `{"norn_control":"hello","protocol":1,"vault_root":"<canonical abs path>"}`
/// - ready: `{"norn_control":"ready","protocol":1,"version":"<semver>"}`
/// - error: `{"norn_control":"error","protocol":1,"message":"..."}`
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "norn_control", rename_all = "lowercase")]
pub enum ControlFrame {
    /// Client -> daemon: "are you alive, and do we speak the same protocol?"
    Ping { protocol: u32 },
    /// Daemon -> client: proof of life plus enough to decide whether to
    /// route. `pid`/`uptime_secs` are informational (useful for `norn service
    /// status`-style diagnostics later); the client today only reads
    /// `version` to gate routing, so they parse as `Option` and a pong
    /// missing them is still a valid, routable pong. `version` is NOT
    /// optional: a pong missing it is a malformed frame (silent Direct), not
    /// a version skew.
    Pong {
        protocol: u32,
        version: String,
        #[serde(default)]
        pid: Option<u32>,
        #[serde(default)]
        uptime_secs: Option<u64>,
    },
    /// Client -> daemon, after the control handshake: names the vault this
    /// connection is for. The daemon derives vault identity from this path
    /// rather than from the socket path (there is only one socket now — see
    /// the module docs). Unused by the client until NRN-94 opens the request
    /// connection; defined now so the wire contract is settled for the
    /// daemon task.
    // Parsed by the unix-only `norn serve` daemon (`src/serve/`); dead on
    // non-unix builds where the daemon can't run.
    #[cfg_attr(not(unix), allow(dead_code))]
    Hello { protocol: u32, vault_root: String },
    /// Daemon -> client: the named vault's warm cache is ready to serve.
    // Constructed by the unix-only `norn serve` daemon; dead on non-unix builds.
    #[cfg_attr(not(unix), allow(dead_code))]
    Ready { protocol: u32, version: String },
    /// Daemon -> client: the request (handshake or named-vault open) failed
    /// on the daemon side.
    // Constructed by the unix-only `norn serve` daemon; dead on non-unix builds.
    #[cfg_attr(not(unix), allow(dead_code))]
    Error { protocol: u32, message: String },
}

/// A handshake outcome that distinguishes version skew from every other
/// failure mode.
///
/// Every handshake failure maps to [`RouteDecision::Direct`] — trust is never
/// skipped just because routing didn't happen — but version skew is the one
/// failure worth telling the operator about: it is actionable (the fix is to
/// restart the `norn serve` daemon) and silent about it would leave the CLI quietly
/// falling back to Direct forever after a client upgrade, with no signal that
/// a stale daemon is the reason. Every other variant (`Other`) covers
/// timeouts, I/O errors, protocol mismatches, and malformed frames, which
/// stay silent because they are expected transient or environmental noise.
//
// This whole probe path is live on unix — NRN-94 wired `route_count` → `probe`
// in `src/lib.rs`, so `probe_socket`/`connect_control`/`handshake`/
// `handshake_pong` (all `#[cfg(unix)]`, absent on non-unix) need no dead-code
// allow. Only the two cross-platform items — [`RouteDecision`] and [`probe`] —
// are unreachable on the non-unix build (the non-unix `route_count` stub never
// calls them), so those two carry a `cfg_attr(not(unix), …)` allow.
// `HandshakeError::Other`'s wrapped error is deliberately never read (callers
// only distinguish it from `VersionSkew`).
#[cfg(unix)]
#[derive(Debug)]
enum HandshakeError {
    /// The daemon answered with a well-formed pong at the right protocol
    /// version, but its build version doesn't match this client's.
    VersionSkew { server: String, client: String },
    /// Anything else: timeout, I/O error, protocol mismatch, wrong frame
    /// kind, or a pong missing `version`. The wrapped error is never read
    /// today — callers only distinguish this variant from `VersionSkew`,
    /// deliberately staying silent about the specifics — but it's kept
    /// rather than discarded so future diagnostics (e.g. `-v` logging) have
    /// it to hand without re-plumbing the type.
    #[allow(dead_code)]
    Other(anyhow::Error),
}

/// How long the probe waits for the handshake pong before giving up and
/// falling back to Direct. Kept short: a live daemon answers the control ping
/// off its accept loop essentially instantly, so a slow reply means "hung",
/// and the cost of a false-negative (running Direct when a daemon was merely
/// slow) is only the loss of the warm-cache speedup, never a correctness or
/// trust loss.
pub const DEFAULT_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// Undocumented env override for the handshake probe timeout, in milliseconds
/// (NRN-94 review F8). The 250ms [`DEFAULT_HANDSHAKE_TIMEOUT`] is tuned for an
/// idle developer machine; under heavy CI load a live daemon can be scheduled
/// late enough that the probe times out and every routed read silently falls
/// back to Direct — which the per-shape routing proof (`serve_count_routing`)
/// now catches as a hard failure rather than a silent pass. This lets the e2e
/// suite set a generous budget so scheduler jitter can't flake it. Deliberately
/// undocumented: it exists for tests and emergency operator use, not as a
/// supported knob.
const HANDSHAKE_TIMEOUT_ENV: &str = "NORN_SERVICE_HANDSHAKE_TIMEOUT_MS";

/// The probe handshake timeout, honoring [`HANDSHAKE_TIMEOUT_ENV`] when it parses
/// to a positive integer, otherwise [`DEFAULT_HANDSHAKE_TIMEOUT`].
pub fn handshake_timeout() -> std::time::Duration {
    match std::env::var(HANDSHAKE_TIMEOUT_ENV) {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(ms) if ms > 0 => std::time::Duration::from_millis(ms),
            _ => DEFAULT_HANDSHAKE_TIMEOUT,
        },
        Err(_) => DEFAULT_HANDSHAKE_TIMEOUT,
    }
}

/// The OVERALL wall-clock budget for one routed read (NRN-94 review F3).
///
/// This is a *tight overall deadline*, not a generous per-read timeout: connect,
/// the `hello`/`ready` preamble, MCP `initialize`, and the `tools/call` all share
/// it, so a routed read costs at most this before falling back to a verified
/// direct open. The old 30s per-read timeout let a daemon that passed the 250ms
/// liveness ping but then wedged in its serve path stall EVERY count for 30s,
/// while direct execution costs ~50ms.
///
/// 5s is the deliberate balance: a genuinely cold daemon open (first touch,
/// integrity check, index build) can legitimately take a second or two and
/// should still route; but if it exceeds 5s, falling back to Direct is nearly
/// free because Direct pays a comparable cold-open cost anyway — and a truly
/// wedged daemon then costs at most 5s, once per invocation, instead of 30s.
#[cfg(unix)]
const ROUTED_READ_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// Per-read `SO_RCVTIMEO` for the request path: short enough that the reader
/// wakes to re-check the [`ROUTED_READ_BUDGET`] deadline, bounding total
/// overshoot to one interval, without the per-read `setsockopt` churn that trips
/// EINVAL on macOS under load (so we set it ONCE, not per read).
#[cfg(unix)]
const REQUEST_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// Upper bound on a single request-path frame (NRN-94 review F2). Far larger
/// than [`MAX_CONTROL_FRAME_BYTES`] because a tool response (e.g. a large
/// `vault.count` grouping envelope) is legitimately bigger than a control frame,
/// but still bounded so a peer that streams bytes without a newline becomes a
/// bounded `Err` (→ Direct) rather than unbounded memory growth.
#[cfg(unix)]
const MAX_REQUEST_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Upper bound on blank/non-JSON noise lines skipped while looking for the next
/// JSON value on the request path (NRN-94 review F2). A well-behaved daemon
/// interleaves none; a chatty or hostile peer cannot loop us forever.
#[cfg(unix)]
const MAX_SKIPPED_FRAMES: usize = 1024;

/// The routing decision for one CLI invocation.
// Cross-platform, but unreachable on non-unix (its `route_count` stub never
// probes); allow dead code only there (see the note on `HandshakeError`).
#[cfg_attr(not(unix), allow(dead_code))]
pub enum RouteDecision {
    /// No live service — run the operation directly with a verified cache open.
    Direct,
    /// A live service answered the handshake; route the request to it.
    #[cfg(unix)]
    Route(ServiceClient),
}

/// A proven-live routing target for a vault's warm service.
///
/// Holding this is proof that a daemon answered the control handshake. The
/// handshake rides a *dedicated control path* (per the design-of-record) that is
/// separate from the request channel, so the control connection is dropped once
/// liveness is proven; NRN-94 opens the request connection to [`socket_path`]
/// with its own I/O budget. Reconnecting fails safe: if the service dies in the
/// gap, the request connect fails and the CLI falls back to a verified direct
/// open — trust is re-established, never skipped.
///
/// [`socket_path`]: ServiceClient::socket_path
#[cfg(unix)]
pub struct ServiceClient {
    /// The socket a handshake just succeeded against, and where the request
    /// connection ([`ServiceClient::call_tool_structured_phased`]) reconnects.
    pub socket_path: Utf8PathBuf,
}

/// The host daemon's run directory: `<XDG_CACHE_HOME>/norn/run`.
///
/// This is where the well-known control socket and its advisory lock live.
/// The client only ever stats or connects under this directory — creating it
/// is the daemon's job (it must exist before the daemon can bind the socket),
/// so this function does not create anything on disk.
pub fn run_dir() -> anyhow::Result<Utf8PathBuf> {
    Ok(crate::cache::cache_tree_root()?.join("run"))
}

/// The host daemon's well-known control socket: `<run_dir>/norn.sock`.
///
/// Fixed and singular — there is exactly one of these per host, independent
/// of how many vaults are open. See the module docs' "one host daemon, not
/// one per vault" section for why this replaced per-vault derivation.
pub fn host_socket_path() -> anyhow::Result<Utf8PathBuf> {
    Ok(run_dir()?.join("norn.sock"))
}

/// The host daemon's advisory lock file: `<run_dir>/norn.lock`, used to keep
/// at most one daemon instance bound to [`host_socket_path`] at a time.
///
/// Acquired on startup by the unix-only `norn serve` daemon (`src/serve/`); dead
/// on non-unix builds where the daemon can't run.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn host_lock_path() -> anyhow::Result<Utf8PathBuf> {
    Ok(run_dir()?.join("norn.lock"))
}

/// Probe for a live host service and decide how to run the request.
///
/// Never errors: any failure to derive the path, stat, connect, or handshake
/// resolves to [`RouteDecision::Direct`] — the always-safe path. The daemon is
/// a pure optimization, so its absence or malfunction must degrade to today's
/// behavior, never surface as an error.
// Cross-platform, but unreachable on non-unix (see the note on `HandshakeError`).
#[cfg_attr(not(unix), allow(dead_code))]
pub fn probe(timeout: std::time::Duration) -> RouteDecision {
    let Ok(socket_path) = host_socket_path() else {
        return RouteDecision::Direct;
    };
    probe_socket(&socket_path, timeout)
}

/// The live daemon's pong details, kept for `norn service status` (NRN-115).
/// Unlike [`probe`] — which discards everything but the routing decision — this
/// preserves `version`/`pid`/`uptime_secs` so status can render running-vs-on-disk.
#[cfg(unix)]
#[derive(Debug, Clone)]
pub struct ServicePong {
    pub version: String,
    pub pid: Option<u32>,
    pub uptime_secs: Option<u64>,
}

/// Control-ping a specific socket for `norn service status`: connect and run
/// the ONE ping/pong exchange ([`handshake_pong`] — shared with the routing
/// probe so the wire exchange cannot drift). `None` on any failure (no socket,
/// no daemon, timeout, garbage) — status then renders "no answer on the
/// control socket".
///
/// Gates: a pong at a foreign [`CONTROL_PROTOCOL`] is `None` — a daemon
/// speaking a different control wire is not a healthy daemon, and rendering
/// its self-reported fields as healthy state would mislead. The VERSION is
/// deliberately NOT gated (unlike the routing probe): status's whole job is to
/// report a version skew as restart-pending, so a stale-but-protocol-compatible
/// daemon's pong must come through.
///
/// The caller supplies the socket path (the command layer probes the SAME path
/// its report prints), which also lets tests point this at a stub listener.
#[cfg(unix)]
pub fn probe_status_socket(
    socket_path: &Utf8Path,
    timeout: std::time::Duration,
) -> Option<ServicePong> {
    if !socket_path.exists() {
        return None;
    }
    // Connect and handshake share one wall-clock budget, like `probe_socket`.
    let deadline = std::time::Instant::now() + timeout;
    let stream = connect_control(socket_path, timeout).ok()?;
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    let (protocol, pong) = handshake_pong(&stream, remaining).ok()?;
    if protocol != CONTROL_PROTOCOL {
        return None;
    }
    Some(pong)
}

/// The ONE operator-actionable routing notice: the daemon's build version does
/// not match this client's. Shared by the probe path and the request-connection
/// gate (NRN-222 review) so operators and log scrapers see one exact line
/// format. Version skew is worth a line — it's actionable and otherwise
/// invisible (the CLI would quietly run Direct forever after a client upgrade).
#[cfg(unix)]
fn warn_version_skew(server: &str, client: &str) {
    eprintln!("norn: service is v{server}, client is v{client} — restart the norn serve daemon");
}

/// Upper bound on the control-frame response size. The pong is a few dozen
/// bytes; this cap turns a peer that streams bytes without a newline into a
/// bounded `Err` (→ Direct) instead of an unbounded buffer growth.
#[cfg(unix)]
pub(crate) const MAX_CONTROL_FRAME_BYTES: usize = 8 * 1024;

/// Probe a specific socket path. Split from [`probe`] so tests can point it at a
/// stub listener on a temp path.
#[cfg(unix)]
pub fn probe_socket(socket_path: &Utf8Path, timeout: std::time::Duration) -> RouteDecision {
    // Fast path: no socket file => no daemon. Just a stat; the common case pays
    // nothing beyond it. `exists()` follows symlinks and swallows errors as
    // false, which is the behavior we want (treat any doubt as "no service").
    if !socket_path.exists() {
        return RouteDecision::Direct;
    }

    // A socket file is present. The connect and the handshake share ONE
    // wall-clock budget (`timeout`): compute the deadline once, spend part of
    // it on the connect, and hand the handshake whatever remains — so a slow
    // connect plus a slow handshake can never exceed `timeout` in total.
    //
    // The connect is bounded (nonblocking connect + `poll(2)` — see
    // `connect_control`): a stale/orphaned socket (daemon died without
    // unlinking) refuses, an over-length path (macOS `sun_path` is ~104 bytes)
    // is rejected, and a wedged daemon whose accept backlog is full times out
    // instead of blocking forever. All three map to Direct.
    let deadline = std::time::Instant::now() + timeout;
    let connect_budget = deadline.saturating_duration_since(std::time::Instant::now());
    let Ok(stream) = connect_control(socket_path, connect_budget) else {
        return RouteDecision::Direct;
    };

    let handshake_budget = deadline.saturating_duration_since(std::time::Instant::now());
    let decision = match handshake(&stream, handshake_budget) {
        Ok(()) => RouteDecision::Route(ServiceClient {
            socket_path: socket_path.to_owned(),
        }),
        // Version skew is the one failure mode worth telling the operator
        // about — it's actionable and otherwise invisible (the CLI would
        // just quietly run Direct forever after a client upgrade). Exactly
        // one line, then fall back like every other failure.
        Err(HandshakeError::VersionSkew { server, client }) => {
            warn_version_skew(&server, &client);
            RouteDecision::Direct
        }
        // Hung (timeout), refused mid-handshake, protocol mismatch, or
        // garbage — all fall back to a verified direct open, silently. Trust
        // is never skipped.
        Err(HandshakeError::Other(_)) => RouteDecision::Direct,
    };
    // The control connection has served its purpose (proving liveness); drop it
    // explicitly. NRN-94 opens the request connection to the same socket.
    drop(stream);
    decision
}

/// Non-Unix stub: no UDS, so routing is never available; always run Direct.
#[cfg(not(unix))]
// Unreachable on non-unix (only `probe` calls it, and `probe` is itself dead
// here — see the note on `HandshakeError`).
#[allow(dead_code)]
pub fn probe_socket(_socket_path: &Utf8Path, _timeout: std::time::Duration) -> RouteDecision {
    RouteDecision::Direct
}

/// Connect to the control socket within `timeout`, returning a
/// blocking-mode [`UnixStream`] ready for the handshake.
///
/// `std`'s blocking connect has no timeout: a wedged daemon whose accept
/// backlog is full blocks it indefinitely on Linux (macOS refuses a full
/// backlog outright). Bounding that is NRN-92 review finding F2. The fix is a
/// nonblocking connect fenced by `poll(2)`:
///
/// 1. `socket(AF_UNIX, SOCK_STREAM)`, wrapped in an [`OwnedFd`] immediately so
///    every early-return path closes the fd — no leak on any error.
/// 2. `FD_CLOEXEC` (macOS has no `SOCK_CLOEXEC` socket() flag, so it is set via
///    `fcntl` unconditionally) and `O_NONBLOCK`, both via `fcntl`.
/// 3. `connect`: immediate success, or `EINPROGRESS`/`EAGAIN` → `poll` for
///    `POLLOUT` bounded by `timeout`. A poll timeout maps to
///    [`std::io::ErrorKind::TimedOut`]; once poll fires, `SO_ERROR` is read as
///    the authoritative async-connect result (non-zero → fail, covering
///    `POLLERR`/`POLLHUP`).
/// 4. Clear `O_NONBLOCK` — the handshake that follows relies on *blocking* I/O
///    bounded by `SO_RCVTIMEO`/`SO_SNDTIMEO`, which nonblocking mode defeats.
///
/// An over-length path (past `sun_path`, ~104 bytes on macOS) is rejected up
/// front rather than truncated (truncation would silently connect to the wrong
/// socket); a refused/stale socket errors too. Both map to Direct at the call
/// site.
///
/// Implemented on `libc`, not `socket2`: a prior `socket2` attempt caused
/// nonblocking-state flakiness and macOS `setsockopt`-under-load `EINVAL`. And
/// `poll`, not `select`, so there is no `FD_SETSIZE` (1024-fd) hazard.
#[cfg(unix)]
fn connect_control(
    socket_path: &Utf8Path,
    timeout: std::time::Duration,
) -> std::io::Result<std::os::unix::net::UnixStream> {
    use std::io::{Error, ErrorKind};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::net::UnixStream;

    // Build the target address, rejecting an over-length path before touching
    // any fd. `sun_path` is a fixed C array; the path plus its trailing NUL
    // must fit, so require `len < sun_path.len()` — the array starts zeroed, so
    // leaving the final byte untouched supplies the NUL terminator.
    let path_bytes = socket_path.as_str().as_bytes();
    // SAFETY: `sockaddr_un` is plain-old-data; an all-zero value is a valid,
    // fully-initialized instance.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if path_bytes.len() >= addr.sun_path.len() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "control socket path exceeds the sun_path limit",
        ));
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (dst, &src) in addr.sun_path.iter_mut().zip(path_bytes) {
        *dst = src as libc::c_char;
    }

    // Create the socket and take ownership immediately: the `OwnedFd` closes the
    // fd on every early return below, so no error path can leak it.
    // SAFETY: `socket(2)` with valid constants; returns -1 on failure.
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw < 0 {
        return Err(Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh, valid, exclusively-owned fd from `socket(2)`.
    let owned = unsafe { OwnedFd::from_raw_fd(raw) };
    let fd = owned.as_raw_fd();

    // FD_CLOEXEC so the fd does not leak across an exec. macOS has no
    // `SOCK_CLOEXEC` socket() flag, so set it here unconditionally.
    // SAFETY: F_GETFD/F_SETFD on a valid fd; both return -1 on failure.
    let fdflags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if fdflags < 0 {
        return Err(Error::last_os_error());
    }
    // SAFETY: F_SETFD on a valid fd.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, fdflags | libc::FD_CLOEXEC) } < 0 {
        return Err(Error::last_os_error());
    }

    // O_NONBLOCK so `connect(2)` returns immediately (EINPROGRESS) instead of
    // blocking on a full accept backlog. `orig_fl` is the pre-nonblocking flag
    // set, restored (cleared) once the connect completes.
    // SAFETY: F_GETFL on a valid fd.
    let orig_fl = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if orig_fl < 0 {
        return Err(Error::last_os_error());
    }
    // SAFETY: F_SETFL on a valid fd.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, orig_fl | libc::O_NONBLOCK) } < 0 {
        return Err(Error::last_os_error());
    }

    // Nonblocking connect.
    // SAFETY: `connect(2)` with a correctly-initialized `sockaddr_un` and the
    // full struct size as the address length (valid for a pathname socket that
    // fits within `sun_path`, verified above).
    let rc = unsafe {
        libc::connect(
            fd,
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        let err = Error::last_os_error();
        // Anything but "in progress" is a hard failure — e.g. ECONNREFUSED for
        // a stale socket, or a full backlog on macOS. Surface it (→ Direct).
        let os = err.raw_os_error();
        if os != Some(libc::EINPROGRESS) && os != Some(libc::EAGAIN) {
            return Err(err);
        }
        // Wait for the connect to settle, bounded by a wall-clock deadline so
        // EINTR restarts cannot extend the budget past `timeout`.
        let deadline = std::time::Instant::now() + timeout;
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(Error::new(
                    ErrorKind::TimedOut,
                    "control-socket connect timed out",
                ));
            }
            let ms = remaining.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
            // SAFETY: `poll(2)` over one initialized `pollfd`.
            let pr = unsafe { libc::poll(&mut pfd, 1, ms) };
            if pr < 0 {
                let e = Error::last_os_error();
                if e.raw_os_error() == Some(libc::EINTR) {
                    continue; // interrupted; re-poll on the remaining budget
                }
                return Err(e);
            }
            if pr == 0 {
                return Err(Error::new(
                    ErrorKind::TimedOut,
                    "control-socket connect timed out",
                ));
            }
            break;
        }
        // poll fired (POLLOUT and/or POLLERR/POLLHUP). SO_ERROR is the
        // authoritative async-connect result; non-zero means it failed.
        let mut soerr: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: `getsockopt` writes one `int` into `soerr`; `len` carries its
        // size and is updated in place.
        let gr = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                &mut soerr as *mut libc::c_int as *mut libc::c_void,
                &mut len,
            )
        };
        if gr < 0 {
            return Err(Error::last_os_error());
        }
        if soerr != 0 {
            return Err(Error::from_raw_os_error(soerr));
        }
    }

    // Restore blocking mode: the handshake relies on blocking reads/writes
    // bounded by SO_RCVTIMEO/SO_SNDTIMEO, which O_NONBLOCK would defeat.
    // SAFETY: F_SETFL on a valid fd, clearing exactly the O_NONBLOCK bit we set.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, orig_fl & !libc::O_NONBLOCK) } < 0 {
        return Err(Error::last_os_error());
    }

    // Transfer ownership into the `UnixStream` (no dup, no leak): `owned` is
    // moved, so the fd is now owned solely by the returned stream.
    Ok(UnixStream::from(owned))
}

/// Exchange the control ping/pong on a connected stream within `timeout`.
///
/// Writes a single newline-delimited JSON [`ControlFrame::Ping`], then reads
/// one line and requires a protocol-matching, exact-version-matching
/// [`ControlFrame::Pong`]. Any I/O error, timeout, protocol mismatch, or
/// unexpected/malformed frame is [`HandshakeError::Other`]; a well-formed,
/// protocol-matching pong whose `version` differs from this build's is
/// [`HandshakeError::VersionSkew`] — the one case the caller surfaces to the
/// operator before falling back to Direct.
///
/// The stream is used only to prove liveness and is dropped by the caller
/// afterward, so the short handshake timeouts never leak onto a request channel.
#[cfg(unix)]
fn handshake(
    stream: &std::os::unix::net::UnixStream,
    timeout: std::time::Duration,
) -> Result<(), HandshakeError> {
    let (protocol, pong) = handshake_pong(stream, timeout).map_err(HandshakeError::Other)?;
    let client_version = env!("CARGO_PKG_VERSION");

    // FIX-8: compare the VERSION first. A well-formed pong at a *different*
    // version is a VersionSkew regardless of protocol — otherwise a future
    // `CONTROL_PROTOCOL` bump would short-circuit on the protocol check and
    // silently map an old daemon to Direct, hiding exactly the skew the stderr
    // note exists to report. Routing still requires BOTH to match.
    if pong.version != client_version {
        return Err(HandshakeError::VersionSkew {
            server: pong.version,
            client: client_version.to_string(),
        });
    }

    // Same version but a different protocol is something weirder than staleness
    // (a same-build daemon speaking a different wire) — fall back to Direct
    // silently, as before.
    if protocol != CONTROL_PROTOCOL {
        return Err(HandshakeError::Other(anyhow::anyhow!(
            "control protocol mismatch: service spoke {protocol}, client wants {CONTROL_PROTOCOL}"
        )));
    }

    Ok(())
}

/// Write the ping, read the reply, and return the pong's protocol plus its
/// payload ([`ServicePong`]). The ONE ping/pong implementation: the routing
/// probe ([`handshake`]) compares protocol+version to decide routing vs.
/// version skew vs. protocol mismatch; `norn service status`
/// ([`probe_status_socket`]) keeps the payload (version/pid/uptime) for the
/// operator report. Every failure short of a well-formed pong (I/O, timeout,
/// wrong frame kind, missing `version`) is a plain `anyhow::Error`;
/// [`handshake`] owns the version-then-protocol ordering that distinguishes
/// skew from a silent protocol mismatch (FIX-8).
#[cfg(unix)]
fn handshake_pong(
    stream: &std::os::unix::net::UnixStream,
    timeout: std::time::Duration,
) -> anyhow::Result<(u32, ServicePong)> {
    use std::io::BufReader;

    let deadline = std::time::Instant::now() + timeout;
    // Set the I/O timeouts once. `SO_RCVTIMEO` bounds each read; the reader's
    // wall-clock deadline check bounds the *total*, so a single set suffices —
    // and setting it once (rather than per read) avoids the repeated setsockopt
    // that spuriously trips EINVAL on macOS under heavy concurrency.
    stream.set_write_timeout(Some(timeout))?;
    stream.set_read_timeout(Some(timeout))?;

    // All I/O rides the borrowed stream directly — `&UnixStream` implements
    // `Read`/`Write`, so no `try_clone` is needed. (Cloning then dropping a
    // dup'd socket fd before touching `SO_RCVTIMEO` on the original trips EINVAL
    // on macOS; borrowing sidesteps that and avoids the fd churn.) The probe and
    // request paths share ONE capped, deadline-aware reader ([`read_line_capped`])
    // and ONE writer ([`write_json_line`]) — see NRN-94 review F2.
    let ping = ControlFrame::Ping {
        protocol: CONTROL_PROTOCOL,
    };
    write_json_line(stream, &ping)?;

    // Read one control line, bounded by the *cumulative* deadline (not per read
    // syscall) and by [`MAX_CONTROL_FRAME_BYTES`], so a trickle of bytes without
    // a newline cannot hang the probe or grow memory unbounded.
    let mut reader = BufReader::new(stream);
    let line = read_line_capped(&mut reader, MAX_CONTROL_FRAME_BYTES, deadline)?
        .ok_or_else(|| anyhow::anyhow!("service closed the connection before answering"))?;

    // A pong missing `version` fails to deserialize here (it's a required
    // field) and lands in the generic `Err` path below — a malformed frame,
    // not a version skew.
    let frame: ControlFrame = serde_json::from_slice(&line)?;
    match frame {
        // Return the fields raw; `handshake` applies the version-first,
        // protocol-second ordering (FIX-8), `probe_status_socket` the
        // protocol-only gate.
        ControlFrame::Pong {
            protocol,
            version,
            pid,
            uptime_secs,
        } => Ok((
            protocol,
            ServicePong {
                version,
                pid,
                uptime_secs,
            },
        )),
        other => anyhow::bail!("unexpected control frame: {other:?}"),
    }
}

/// Read a single newline-terminated line from a buffered reader, bounded by a
/// wall-clock `deadline` and `cap` bytes. Returns `Ok(None)` on EOF before any
/// byte, `Ok(Some(line))` (newline stripped), or `Err` on a cap/deadline breach.
///
/// This is the ONE line reader shared by the probe handshake and the request
/// path (NRN-94 review F2) — previously two divergent copies, one of which (the
/// request path's `read_line`) had neither a byte cap nor a cumulative deadline.
///
/// The caller sets `SO_RCVTIMEO` (bounding each individual read); this loop's
/// deadline check bounds the *cumulative* time — the key fix over a plain
/// `read_line`, which a peer that dribbles bytes under the per-read timeout can
/// keep alive forever. A per-read timeout (`WouldBlock`/`TimedOut`) or a stray
/// signal (`Interrupted`) is retried, always re-gated by the deadline, so the
/// only ways out are: a full line, EOF, the cap, or the deadline.
///
/// Uses `fill_buf`/`consume` so it never over-reads past the newline — any
/// pipelined bytes stay buffered for the next call (the request path reads
/// several frames from one `BufReader`).
#[cfg(unix)]
fn read_line_capped<R: std::io::BufRead>(
    reader: &mut R,
    cap: usize,
    deadline: std::time::Instant,
) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::{Error, ErrorKind};

    let mut buf: Vec<u8> = Vec::new();
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(Error::new(
                ErrorKind::TimedOut,
                "deadline exceeded before a full line arrived",
            ));
        }
        let available = match reader.fill_buf() {
            Ok(a) => a,
            // A per-read `SO_RCVTIMEO` expiry or a stray signal: loop and let the
            // deadline check above decide whether to keep waiting.
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::Interrupted | ErrorKind::WouldBlock | ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            // Clean EOF before any newline: a partial buffer is discarded.
            return Ok(None);
        }
        // Bound how much of `available` we look at this iteration so `buf` can
        // overshoot `cap` by at most one byte before the cap trips — never by a
        // whole extra `fill_buf` chunk. `buf.len() <= cap` is the invariant, so
        // this budget is always >= 1.
        let budget = cap - buf.len() + 1;
        let window = &available[..available.len().min(budget)];
        if let Some(pos) = window.iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&window[..pos]);
            reader.consume(pos + 1);
            return Ok(Some(buf));
        }
        let taken = window.len();
        buf.extend_from_slice(window);
        reader.consume(taken);
        if buf.len() > cap {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "line exceeded the byte cap without a newline",
            ));
        }
    }
}

/// Where a routed tool call failed, relative to the moment the `tools/call`
/// frame crosses to the daemon (NRN-228).
///
/// For a read this distinction is invisible (every failure falls back to a
/// verified direct open either way). For a *mutation* it is the whole ballgame:
/// a pre-send failure never reached the tool, so a Direct retry is safe; a
/// post-send failure may have applied, so a Direct retry could double-apply.
/// [`ServiceClient::call_tool_structured_phased`] tags every failure with one of
/// these so a send-commit policy (see the routing seam's `route_call`) can decide.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallPhase {
    /// The `tools/call` frame was never written — the failure happened during
    /// the socket-trust check, the request connect, the `hello`/`ready`
    /// preamble, the request-connection version gate, or the MCP `initialize`.
    /// The tool never ran, so re-running Direct cannot double-apply.
    PreSend,
    /// The `tools/call` frame was written; the daemon may have executed the
    /// tool. The failure is a lost/garbled/timed-out response — NOT proof the
    /// tool did not run — so a mutation must not silently retry Direct.
    PostSend,
}

/// A routed tool-call failure carrying its [`CallPhase`] (NRN-228). The read
/// path collapses this to a plain `anyhow::Error` (it falls back regardless);
/// the mutation path branches on `phase`.
#[cfg(unix)]
#[derive(Debug)]
pub struct CallToolError {
    /// Whether the tool request had already been sent when the failure hit.
    pub phase: CallPhase,
    /// The underlying failure, preserved verbatim for the operator-facing message.
    pub source: anyhow::Error,
}

#[cfg(unix)]
impl CallToolError {
    /// Tag a failure that struck before the `tools/call` frame was written.
    fn pre_send(source: impl Into<anyhow::Error>) -> Self {
        Self {
            phase: CallPhase::PreSend,
            source: source.into(),
        }
    }

    /// Tag a failure that struck at or after the `tools/call` frame was written.
    fn post_send(source: impl Into<anyhow::Error>) -> Self {
        Self {
            phase: CallPhase::PostSend,
            source: source.into(),
        }
    }
}

#[cfg(unix)]
impl std::fmt::Display for CallToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.source)
    }
}

#[cfg(unix)]
impl std::error::Error for CallToolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// The request half of the routing seam (NRN-94).
///
/// A [`ServiceClient`] is proof that a daemon answered the liveness ping on a
/// *separate* control connection. This opens a fresh REQUEST connection to the
/// same socket — under the tight overall [`ROUTED_READ_BUDGET`] deadline, not the
/// probe's short handshake timeout — verifies the socket is ours, sends the
/// `hello` vault preamble, runs the MCP `initialize` + `tools/call` exchange as
/// raw newline-delimited JSON-RPC, and returns the tool's `structuredContent`.
///
/// Every failure — a socket that fails the ownership check (a squatter on the
/// well-known path), connect refused (daemon died in the probe→request gap), a
/// non-`ready` preamble reply (vault open failed daemon-side), an MCP transport
/// error, a JSON-RPC `error`, or a result flagged `isError` (the tool itself
/// failed) — is returned as `Err`. The caller maps ALL of them to a verified
/// direct open, so a routed read never fails a read that direct execution could
/// serve. Because reads are idempotent and side-effect-free, this
/// attempt-then-fall-back is safe: a tool-level error (e.g. an invalid `--by`) is
/// re-produced identically by the direct path, so error output and exit codes
/// stay byte-identical too.
///
/// The one carve-out is [`OnToolError::AcceptWithPayload`]: a tool like
/// `vault.get` uses `isError: true` as a *semantic* signal (a requested target
/// did not resolve — the MCP twin of the CLI's exit 1, NRN-214) while still
/// returning the full `structuredContent`. For such a tool, the routed client
/// accepts the flagged result and reproduces the CLI failure contract from the
/// payload, instead of treating it as a transport failure and re-executing the
/// whole read directly.
#[cfg(unix)]
impl ServiceClient {
    /// Route one read tool call to the warm daemon and return its
    /// `structuredContent` JSON. `vault_root` is the canonical vault root the
    /// caller computed once (the wire speaks canonical paths only — ADR 0005);
    /// `tool` is the MCP tool name (e.g. `"vault.count"`); `arguments` is the
    /// tool's parameter object; `on_tool_error` decides what a result flagged
    /// `isError: true` means (see [`OnToolError`]).
    /// Route one tool call to the warm daemon and return its `structuredContent`
    /// JSON, tagging every failure with the [`CallPhase`] about the moment the
    /// `tools/call` frame is written (the send boundary). A read discards the
    /// phase (it falls back to Direct on any failure — see `route_read`); a
    /// mutation branches on it, refusing to fall back once the request has
    /// crossed to the daemon (see `route_call`). NRN-228.
    pub fn call_tool_structured_phased(
        &self,
        vault_root: &Utf8Path,
        tool: &str,
        arguments: serde_json::Value,
        on_tool_error: OnToolError,
    ) -> Result<serde_json::Value, CallToolError> {
        use std::io::BufReader;

        // ── PRE-SEND: nothing below has written the tool call yet, so every
        //    failure here is `CallPhase::PreSend` — the tool never ran. ──

        // TRUST (NRN-94 review F4): before naming the vault, verify the socket is
        // ours. A squatter who binds the well-known path must not learn vault
        // paths (from the `hello`) or serve forged counts. On any mismatch, bail
        // so the caller falls back to a verified direct open — trust over speed.
        if !socket_is_trusted(&self.socket_path) {
            return Err(CallToolError::pre_send(anyhow::anyhow!(
                "service socket failed the ownership check; using direct execution"
            )));
        }

        // One overall wall-clock deadline for the whole routed read (F3): a
        // daemon that passed the 250ms liveness ping but then wedges costs at
        // most the budget, not the old 30s. Every read below shares this deadline.
        let deadline = std::time::Instant::now() + ROUTED_READ_BUDGET;

        // Fresh request connection, bounded by the same budget.
        let stream = connect_control(&self.socket_path, ROUTED_READ_BUDGET)
            .map_err(CallToolError::pre_send)?;
        // `SO_RCVTIMEO` is the per-read poll interval (short, so the reader wakes
        // to re-check the deadline); the deadline bounds the cumulative time. Set
        // once — per-read `setsockopt` trips EINVAL on macOS under load.
        stream
            .set_read_timeout(Some(REQUEST_POLL_INTERVAL))
            .map_err(CallToolError::pre_send)?;
        stream
            .set_write_timeout(Some(ROUTED_READ_BUDGET))
            .map_err(CallToolError::pre_send)?;
        // Read via a BufReader over `&stream`; write via `&stream` directly. Both
        // are shared borrows, so no `try_clone` (whose dup'd-fd churn trips the
        // macOS `SO_RCVTIMEO` EINVAL noted on the probe path).
        let mut reader = BufReader::new(&stream);

        // 1. Vault preamble: name the vault for this connection, then require a
        //    `ready` frame. Anything else (an `error` frame, EOF, garbage) means
        //    the daemon could not serve this vault — fall back to Direct.
        let hello = ControlFrame::Hello {
            protocol: CONTROL_PROTOCOL,
            vault_root: vault_root.as_str().to_string(),
        };
        write_json_line(&stream, &hello).map_err(CallToolError::pre_send)?;
        let ready = read_json_value(&mut reader, deadline)
            .map_err(CallToolError::pre_send)?
            .ok_or_else(|| {
                CallToolError::pre_send(anyhow::anyhow!("daemon closed before answering hello"))
            })?;
        match ready.get("norn_control").and_then(|v| v.as_str()) {
            Some("ready") => {}
            _ => {
                return Err(CallToolError::pre_send(anyhow::anyhow!(
                    "daemon did not answer hello with ready: {ready}"
                )))
            }
        }
        // Version gate on the REQUEST connection too (NRN-222 review): the
        // probe's ping is version-gated, but this is a separate connection — a
        // daemon swapped between probe and request could serve a stale wire
        // shape. Require the ready frame's exact build version; on skew, emit
        // the SAME operator-actionable one-liner the probe path uses, then
        // `Err` so the caller falls back to a verified direct open.
        let client_version = env!("CARGO_PKG_VERSION");
        match ready.get("version").and_then(|v| v.as_str()) {
            Some(version) if version == client_version => {}
            Some(server) => {
                warn_version_skew(server, client_version);
                return Err(CallToolError::pre_send(anyhow::anyhow!(
                    "service/client version skew on the request connection"
                )));
            }
            None => {
                return Err(CallToolError::pre_send(anyhow::anyhow!(
                    "ready frame carries no version; refusing to route"
                )))
            }
        }

        // 2. MCP handshake. The daemon serves rmcp over the remainder of the
        //    stream; `initialize` must succeed before any `tools/call`. Still
        //    pre-send — `initialize` mutates nothing.
        let init = json_rpc_request(
            1,
            "initialize",
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "norn-cli", "version": env!("CARGO_PKG_VERSION") },
            }),
        );
        write_json_line(&stream, &init).map_err(CallToolError::pre_send)?;
        let init_resp =
            read_rpc_response(&mut reader, 1, deadline).map_err(CallToolError::pre_send)?;
        if let Some(err) = init_resp.get("error") {
            return Err(CallToolError::pre_send(anyhow::anyhow!(
                "MCP initialize failed: {err}"
            )));
        }

        // ── SEND BOUNDARY: writing the `tools/call` frame commits the request to
        //    the daemon. From here every failure is `CallPhase::PostSend` — the
        //    tool may have run, so a mutation caller must NOT silently retry
        //    Direct. A JSON-RPC `error` OR a `result` flagged `isError` is a
        //    tool-level failure; for a read the caller falls back to Direct
        //    (which re-produces the identical error and exit code). ──
        let call = json_rpc_request(
            2,
            "tools/call",
            serde_json::json!({ "name": tool, "arguments": arguments }),
        );
        write_json_line(&stream, &call).map_err(CallToolError::post_send)?;
        let resp = read_rpc_response(&mut reader, 2, deadline).map_err(CallToolError::post_send)?;
        if let Some(err) = resp.get("error") {
            return Err(CallToolError::post_send(anyhow::anyhow!(
                "tool '{tool}' failed on the daemon: {err}"
            )));
        }
        let result = resp.get("result").ok_or_else(|| {
            CallToolError::post_send(anyhow::anyhow!("tool '{tool}' returned no result"))
        })?;
        // F5: a successful JSON-RPC envelope can still carry a tool-level failure
        // via `isError: true` (an MCP CallToolResult convention). Under
        // `FallBackDirect`, treat it as a failure so we do not render a
        // forged/error payload as a real result; under `AcceptWithPayload` the
        // flag is that tool's semantic failure signal (e.g. vault.get's
        // not-found → CLI exit 1) and the structuredContent check below still
        // guards against an empty flagged result.
        if on_tool_error == OnToolError::FallBackDirect
            && result.get("isError").and_then(serde_json::Value::as_bool) == Some(true)
        {
            return Err(CallToolError::post_send(anyhow::anyhow!(
                "tool '{tool}' reported isError; using direct execution"
            )));
        }
        let structured = result
            .get("structuredContent")
            .cloned()
            .filter(|v| !v.is_null())
            .ok_or_else(|| {
                CallToolError::post_send(anyhow::anyhow!(
                    "tool '{tool}' returned no structuredContent"
                ))
            })?;
        Ok(structured)
    }
}

/// What a routed read does with a `tools/call` result flagged `isError: true`
/// (NRN-222).
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnToolError {
    /// Treat the flagged result as a routing failure: `Err`, so the caller
    /// falls back to a verified direct open, which re-produces the error (and
    /// its output/exit code) canonically. The right default for tools whose
    /// results never flag semantic errors (`vault.count`, `vault.find`).
    FallBackDirect,
    /// Accept the flagged result as long as it carries `structuredContent` —
    /// for tools (`vault.get`) whose `isError` is a semantic failure signal
    /// riding a complete, renderable payload (the MCP twin of a CLI nonzero
    /// exit). A flagged result WITHOUT `structuredContent` is still an `Err`
    /// (nothing to render — fall back to Direct).
    AcceptWithPayload,
}

/// Whether the socket at `path` is trustworthy to route through (NRN-94 F4):
/// owned by the current effective uid and not world-writable. Cheap (one
/// `stat`), unix-only, best-effort — any doubt (unreadable metadata, wrong
/// owner, world-writable) returns `false` so the caller falls back to Direct.
#[cfg(unix)]
fn socket_is_trusted(path: &Utf8Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(md) = std::fs::metadata(path) else {
        return false;
    };
    // SAFETY: `geteuid(2)` is always successful and takes no arguments.
    let euid = unsafe { libc::geteuid() };
    // Owned by us: a squatter running as another uid is rejected here.
    if md.uid() != euid {
        return false;
    }
    // Refuse a world-writable socket — anyone could have replaced it.
    if md.mode() & 0o002 != 0 {
        return false;
    }
    true
}

/// Build a JSON-RPC 2.0 request frame.
#[cfg(unix)]
fn json_rpc_request(id: u32, method: &str, params: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

/// Write one newline-delimited JSON value to `stream` and flush. The ONE writer
/// shared by the probe handshake and the request path (NRN-94 review F2).
#[cfg(unix)]
fn write_json_line<T: serde::Serialize>(
    mut stream: &std::os::unix::net::UnixStream,
    value: &T,
) -> std::io::Result<()> {
    use std::io::Write;
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    stream.flush()
}

/// Read one newline-delimited JSON value, skipping blank / non-JSON noise lines,
/// bounded by [`MAX_REQUEST_FRAME_BYTES`], the `deadline`, and
/// [`MAX_SKIPPED_FRAMES`] (NRN-94 review F2 — the skip loop was previously
/// unbounded). Returns `Ok(None)` on EOF before any JSON value.
#[cfg(unix)]
fn read_json_value<R: std::io::BufRead>(
    reader: &mut R,
    deadline: std::time::Instant,
) -> anyhow::Result<Option<serde_json::Value>> {
    for _ in 0..MAX_SKIPPED_FRAMES {
        let Some(line) = read_line_capped(reader, MAX_REQUEST_FRAME_BYTES, deadline)? else {
            return Ok(None);
        };
        let trimmed = line
            .iter()
            .position(|b| !b.is_ascii_whitespace())
            .map(|start| &line[start..])
            .unwrap_or(&[]);
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(trimmed) {
            return Ok(Some(v));
        }
        // Non-JSON line on the wire: skip and keep reading (bounded by the loop).
    }
    anyhow::bail!("too many non-JSON frames before a JSON value")
}

/// Read JSON-RPC responses until one matches `id` (skipping notifications and
/// out-of-order frames), bounded by the `deadline` (F3) and a frame count so a
/// chatty peer cannot loop forever.
#[cfg(unix)]
fn read_rpc_response<R: std::io::BufRead>(
    reader: &mut R,
    id: u32,
    deadline: std::time::Instant,
) -> anyhow::Result<serde_json::Value> {
    for _ in 0..10_000 {
        let v = read_json_value(reader, deadline)?
            .ok_or_else(|| anyhow::anyhow!("daemon closed before answering request {id}"))?;
        if v.get("id").and_then(|i| i.as_u64()) == Some(u64::from(id)) {
            return Ok(v);
        }
        // A notification (no id) or a response to a different id: keep reading.
    }
    anyhow::bail!("no JSON-RPC response for request {id}")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;

    /// Bind a stub listener and pin the socket to 0600, so the request path's
    /// F4 trust gate (`socket_is_trusted`) accepts it regardless of the ambient
    /// umask (which could otherwise leave it group/other-writable).
    fn bind_trusted(path: &Utf8PathBuf) -> UnixListener {
        use std::os::unix::fs::PermissionsExt;
        let listener = UnixListener::bind(path).unwrap();
        std::fs::set_permissions(path.as_std_path(), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        listener
    }

    /// The host socket path is deterministic (same process env => same
    /// answer every call) and lands at the well-known `norn/run/norn.sock`
    /// suffix under the ambient cache tree.
    ///
    /// Deliberately does NOT `std::env::set_var` to force a specific
    /// `XDG_CACHE_HOME`: that call is process-global and races other
    /// in-binary tests reading the same env (see the `mcp::server` tests'
    /// `cold_seeded_vault` for the same rule). Asserting the suffix under
    /// whatever cache root the ambient environment resolves to is enough to
    /// prove derivation is well-known and stable without racing anything.
    #[test]
    fn host_socket_path_is_deterministic_and_well_known() {
        let a = host_socket_path().expect("derive");
        let b = host_socket_path().expect("derive again");
        assert_eq!(a, b, "same process env => same host socket path");
        assert!(a.as_str().ends_with("norn/run/norn.sock"), "path was {a}");
    }

    /// NRN-94 request path: a `ServiceClient` sends the `hello` vault preamble,
    /// runs the MCP `initialize` + `tools/call` exchange, and returns the tool's
    /// `structuredContent`. A stub listener replays exactly the wire the real
    /// daemon speaks (ready frame → JSON-RPC over the same stream) and asserts
    /// the client named the vault it was asked to.
    #[test]
    fn call_tool_structured_completes_the_hello_and_mcp_exchange() {
        use std::sync::mpsc;

        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("service.sock")).unwrap();
        let listener = bind_trusted(&path);

        // Channel so the test can assert the vault_root the client sent in hello.
        let (tx, rx) = mpsc::channel::<String>();
        let server = thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut w = conn;

            // hello → ready
            let mut hello_line = String::new();
            reader.read_line(&mut hello_line).unwrap();
            let hello: serde_json::Value = serde_json::from_str(hello_line.trim()).unwrap();
            assert_eq!(hello["norn_control"], "hello");
            tx.send(hello["vault_root"].as_str().unwrap().to_string())
                .unwrap();
            writeln!(
                w,
                "{}",
                serde_json::json!({
                    "norn_control": "ready",
                    "protocol": CONTROL_PROTOCOL,
                    "version": env!("CARGO_PKG_VERSION"),
                })
            )
            .unwrap();
            w.flush().unwrap();

            // initialize (id=1) → result
            let mut init_line = String::new();
            reader.read_line(&mut init_line).unwrap();
            let init: serde_json::Value = serde_json::from_str(init_line.trim()).unwrap();
            assert_eq!(init["method"], "initialize");
            writeln!(
                w,
                "{}",
                serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}})
            )
            .unwrap();
            w.flush().unwrap();

            // tools/call (id=2) → result with structuredContent
            let mut call_line = String::new();
            reader.read_line(&mut call_line).unwrap();
            let call: serde_json::Value = serde_json::from_str(call_line.trim()).unwrap();
            assert_eq!(call["method"], "tools/call");
            assert_eq!(call["params"]["name"], "vault.count");
            writeln!(
                w,
                "{}",
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "result": {
                        "content": [{"type": "text", "text": "{\"total\":3}"}],
                        "structuredContent": {"total": 3},
                        "isError": false,
                    }
                })
            )
            .unwrap();
            w.flush().unwrap();
        });

        let client = ServiceClient {
            socket_path: path.clone(),
        };
        let vault_root = Utf8PathBuf::from("/vaults/atlas");
        let structured = client
            .call_tool_structured_phased(
                &vault_root,
                "vault.count",
                serde_json::json!({"by": "type"}),
                OnToolError::FallBackDirect,
            )
            .expect("routed tool call should succeed");

        assert_eq!(structured, serde_json::json!({"total": 3}));
        assert_eq!(rx.recv().unwrap(), "/vaults/atlas", "hello named the vault");
        server.join().unwrap();
    }

    /// A daemon that answers `hello` with an `error` frame (vault open failed on
    /// its side) surfaces as `Err`, so the caller falls back to Direct.
    #[test]
    fn call_tool_structured_error_preamble_is_err() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("service.sock")).unwrap();
        let listener = bind_trusted(&path);

        let server = thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut w = conn;
            let mut hello_line = String::new();
            reader.read_line(&mut hello_line).unwrap();
            writeln!(
                w,
                "{}",
                serde_json::json!({
                    "norn_control": "error",
                    "protocol": CONTROL_PROTOCOL,
                    "message": "vault not found",
                })
            )
            .unwrap();
            w.flush().unwrap();
        });

        let client = ServiceClient {
            socket_path: path.clone(),
        };
        let result = client.call_tool_structured_phased(
            &Utf8PathBuf::from("/vaults/atlas"),
            "vault.count",
            serde_json::json!({}),
            OnToolError::FallBackDirect,
        );
        assert!(result.is_err(), "an error preamble must be Err (→ Direct)");
        server.join().unwrap();
    }

    /// NRN-222 review: the REQUEST connection is version-gated like the probe.
    /// A daemon swapped between probe and request answers `ready` with a
    /// different build version — the client must refuse (→ Direct) instead of
    /// riding a stale wire shape. A ready frame with NO version is refused too.
    ///
    /// The assertions require the GATE's distinctive error text, not just
    /// `is_err()`: the stub closes the socket after `ready`, so with the gate
    /// deleted the call still errors (a transport failure on `initialize`) —
    /// a bare `is_err()` would pass vacuously.
    #[test]
    fn call_tool_structured_version_skew_on_ready_is_err() {
        for (stale_ready, expected_in_error) in [
            (
                serde_json::json!({
                    "norn_control": "ready", "protocol": CONTROL_PROTOCOL,
                    "version": "0.0.0-stale",
                }),
                "version skew",
            ),
            (
                serde_json::json!({ "norn_control": "ready", "protocol": CONTROL_PROTOCOL }),
                "carries no version",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = Utf8PathBuf::from_path_buf(dir.path().join("service.sock")).unwrap();
            let listener = bind_trusted(&path);

            let server = thread::spawn(move || {
                let (conn, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(conn.try_clone().unwrap());
                let mut w = conn;
                let mut hello_line = String::new();
                reader.read_line(&mut hello_line).unwrap();
                writeln!(w, "{stale_ready}").unwrap();
                w.flush().unwrap();
            });

            let client = ServiceClient {
                socket_path: path.clone(),
            };
            let error = client
                .call_tool_structured_phased(
                    &Utf8PathBuf::from("/vaults/atlas"),
                    "vault.count",
                    serde_json::json!({}),
                    OnToolError::FallBackDirect,
                )
                .expect_err("a mismatched/missing version must be Err (→ Direct)");
            assert!(
                error.to_string().contains(expected_in_error),
                "the error must come from the version GATE (expected {expected_in_error:?}), \
                 not a downstream transport failure; got: {error}"
            );
            server.join().unwrap();
        }
    }

    /// F5: a tools/call whose JSON-RPC envelope is a success (no `error` member)
    /// but whose `result` carries `isError: true` must be treated as a failure —
    /// otherwise a forged/error payload with `structuredContent` renders as a
    /// real count. The stub replays a full, otherwise-valid exchange and only
    /// flips `isError`.
    #[test]
    fn call_tool_structured_is_error_result_is_err() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("service.sock")).unwrap();
        let listener = bind_trusted(&path);

        let server = thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut w = conn;
            // hello → ready
            let mut hello_line = String::new();
            reader.read_line(&mut hello_line).unwrap();
            writeln!(
                w,
                "{}",
                serde_json::json!({
                    "norn_control": "ready", "protocol": CONTROL_PROTOCOL,
                    "version": env!("CARGO_PKG_VERSION"),
                })
            )
            .unwrap();
            w.flush().unwrap();
            // initialize → result
            let mut init_line = String::new();
            reader.read_line(&mut init_line).unwrap();
            writeln!(
                w,
                "{}",
                serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}})
            )
            .unwrap();
            w.flush().unwrap();
            // tools/call → result flagged isError, but WITH structuredContent.
            let mut call_line = String::new();
            reader.read_line(&mut call_line).unwrap();
            writeln!(
                w,
                "{}",
                serde_json::json!({
                    "jsonrpc": "2.0", "id": 2,
                    "result": {
                        "content": [{"type": "text", "text": "boom"}],
                        "structuredContent": {"total": 999},
                        "isError": true,
                    }
                })
            )
            .unwrap();
            w.flush().unwrap();
        });

        let client = ServiceClient {
            socket_path: path.clone(),
        };
        let result = client.call_tool_structured_phased(
            &Utf8PathBuf::from("/vaults/atlas"),
            "vault.count",
            serde_json::json!({}),
            OnToolError::FallBackDirect,
        );
        assert!(
            result.is_err(),
            "isError:true must be Err (→ Direct), even with structuredContent present"
        );
        server.join().unwrap();
    }

    /// NRN-222: under `AcceptWithPayload`, the SAME flagged exchange returns the
    /// structuredContent — the semantic-failure carve-out `vault.get` rides
    /// (its `isError` is the not-found signal, and the payload carries the
    /// notes the CLI's exit-1 derives from).
    #[test]
    fn call_tool_structured_is_error_accepted_with_payload() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("service.sock")).unwrap();
        let listener = bind_trusted(&path);

        let server = thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut w = conn;
            let mut hello_line = String::new();
            reader.read_line(&mut hello_line).unwrap();
            writeln!(
                w,
                "{}",
                serde_json::json!({
                    "norn_control": "ready", "protocol": CONTROL_PROTOCOL,
                    "version": env!("CARGO_PKG_VERSION"),
                })
            )
            .unwrap();
            w.flush().unwrap();
            let mut init_line = String::new();
            reader.read_line(&mut init_line).unwrap();
            writeln!(
                w,
                "{}",
                serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}})
            )
            .unwrap();
            w.flush().unwrap();
            let mut call_line = String::new();
            reader.read_line(&mut call_line).unwrap();
            writeln!(
                w,
                "{}",
                serde_json::json!({
                    "jsonrpc": "2.0", "id": 2,
                    "result": {
                        "content": [{"type": "text", "text": "not found"}],
                        "structuredContent": {"records": [], "notes": ["error: nope"]},
                        "isError": true,
                    }
                })
            )
            .unwrap();
            w.flush().unwrap();
        });

        let client = ServiceClient {
            socket_path: path.clone(),
        };
        let structured = client
            .call_tool_structured_phased(
                &Utf8PathBuf::from("/vaults/atlas"),
                "vault.get",
                serde_json::json!({"targets": ["nope"]}),
                OnToolError::AcceptWithPayload,
            )
            .expect("AcceptWithPayload must take the flagged payload");
        assert_eq!(
            structured,
            serde_json::json!({"records": [], "notes": ["error: nope"]})
        );
        server.join().unwrap();
    }

    /// NRN-228: a failure BEFORE the `tools/call` frame is written is tagged
    /// `CallPhase::PreSend` — the tool never ran. The stub accepts `hello` and
    /// then drops the connection without a `ready` frame, so the client fails at
    /// the preamble. A mutation caller can safely fall back to Direct here.
    #[test]
    fn call_tool_structured_phased_tags_a_preamble_failure_pre_send() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("service.sock")).unwrap();
        let listener = bind_trusted(&path);

        let server = thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut hello_line = String::new();
            reader.read_line(&mut hello_line).unwrap();
            // Drop without answering `ready`: the tool call is never written.
            drop(conn);
        });

        let client = ServiceClient {
            socket_path: path.clone(),
        };
        let error = client
            .call_tool_structured_phased(
                &Utf8PathBuf::from("/vaults/atlas"),
                "vault.set",
                serde_json::json!({}),
                OnToolError::AcceptWithPayload,
            )
            .expect_err("a dropped preamble must be Err");
        assert_eq!(
            error.phase,
            CallPhase::PreSend,
            "a pre-`tools/call` failure must be tagged PreSend (got {error})"
        );
        server.join().unwrap();
    }

    /// NRN-228: a failure AFTER the `tools/call` frame is written is tagged
    /// `CallPhase::PostSend` — the daemon may have run the tool. The stub
    /// completes `hello`/`ready` and `initialize`, reads the `tools/call`, then
    /// drops WITHOUT a response, so the client fails reading the tool result.
    /// A mutation caller must NOT silently retry Direct here.
    #[test]
    fn call_tool_structured_phased_tags_a_dropped_response_post_send() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("service.sock")).unwrap();
        let listener = bind_trusted(&path);

        let server = thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut w = conn;
            // hello → ready
            let mut hello_line = String::new();
            reader.read_line(&mut hello_line).unwrap();
            writeln!(
                w,
                "{}",
                serde_json::json!({
                    "norn_control": "ready", "protocol": CONTROL_PROTOCOL,
                    "version": env!("CARGO_PKG_VERSION"),
                })
            )
            .unwrap();
            w.flush().unwrap();
            // initialize → result
            let mut init_line = String::new();
            reader.read_line(&mut init_line).unwrap();
            writeln!(
                w,
                "{}",
                serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}})
            )
            .unwrap();
            w.flush().unwrap();
            // Read the tools/call, then DROP before responding: the request has
            // crossed to the daemon, but no result comes back.
            let mut call_line = String::new();
            reader.read_line(&mut call_line).unwrap();
            drop(w);
        });

        let client = ServiceClient {
            socket_path: path.clone(),
        };
        let error = client
            .call_tool_structured_phased(
                &Utf8PathBuf::from("/vaults/atlas"),
                "vault.set",
                serde_json::json!({}),
                OnToolError::AcceptWithPayload,
            )
            .expect_err("a dropped response must be Err");
        assert_eq!(
            error.phase,
            CallPhase::PostSend,
            "a post-`tools/call` failure must be tagged PostSend (got {error})"
        );
        server.join().unwrap();
    }

    /// F4: the request path refuses to route through a socket it does not own.
    /// A normally-created socket (owned by us, not world-writable) passes;
    /// making it world-writable fails the trust gate.
    #[test]
    fn socket_trust_gate() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("service.sock")).unwrap();
        let _listener = UnixListener::bind(&path).unwrap();
        // Pin owner-only first so the "trusted" assertion is umask-independent.
        std::fs::set_permissions(path.as_std_path(), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        assert!(
            socket_is_trusted(&path),
            "an owner-only socket we own must be trusted"
        );

        // World-writable => untrusted (a squatter could have replaced it).
        std::fs::set_permissions(path.as_std_path(), std::fs::Permissions::from_mode(0o777))
            .unwrap();
        assert!(
            !socket_is_trusted(&path),
            "a world-writable socket must fail the trust gate"
        );

        // A missing socket is untrusted (metadata fails).
        let missing = Utf8PathBuf::from_path_buf(dir.path().join("nope.sock")).unwrap();
        assert!(
            !socket_is_trusted(&missing),
            "absent socket must be untrusted"
        );
    }

    /// F2: `read_line_capped` bounds a peer that streams bytes without a newline
    /// to a `cap`-sized `Err`, rather than growing memory unbounded. Exercised
    /// with a `Cursor` (never times out) so only the byte cap can stop it.
    #[test]
    fn read_line_capped_enforces_the_byte_cap() {
        use std::io::{BufReader, Cursor};
        let no_newline = vec![b'x'; 100];
        let mut reader = BufReader::new(Cursor::new(no_newline));
        let far = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let err = read_line_capped(&mut reader, 16, far).expect_err("cap must trip");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "got {err:?}");
    }

    /// F2: `read_line_capped` is bounded by the cumulative `deadline`, not just
    /// per-read `SO_RCVTIMEO`. A socketpair peer that sends nothing (and never
    /// closes) must time out near the deadline, not hang.
    #[test]
    fn read_line_capped_enforces_the_deadline() {
        use std::io::BufReader;
        use std::os::unix::net::UnixStream;

        let (a, _b) = UnixStream::pair().unwrap();
        // Short per-read timeout so the reader wakes to re-check the deadline;
        // `_b` is held open so there is no EOF — only the deadline stops us.
        a.set_read_timeout(Some(std::time::Duration::from_millis(40)))
            .unwrap();
        let mut reader = BufReader::new(&a);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(120);
        let start = std::time::Instant::now();
        let err = read_line_capped(&mut reader, MAX_CONTROL_FRAME_BYTES, deadline)
            .expect_err("deadline must trip");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut, "got {err:?}");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "must give up near the 120ms deadline, not hang (elapsed {:?})",
            start.elapsed()
        );
    }

    /// No socket file present => the fast Direct path.
    #[test]
    fn absent_socket_is_direct() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("service.sock")).unwrap();
        assert!(matches!(
            probe_socket(&path, DEFAULT_HANDSHAKE_TIMEOUT),
            RouteDecision::Direct
        ));
    }

    /// A live listener that answers a prompt, well-formed pong at the same
    /// protocol AND the same build version => Route. The version match is
    /// now load-bearing: a pong at the right protocol but a different
    /// `version` must NOT route (see `version_skew_is_direct` below).
    #[test]
    fn live_service_answering_pong_is_routed() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("service.sock")).unwrap();
        let listener = UnixListener::bind(&path).unwrap();

        let server = thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            // Answer with a matching-protocol, matching-version pong,
            // including the informational pid/uptime fields a real daemon
            // sends.
            let pong = ControlFrame::Pong {
                protocol: CONTROL_PROTOCOL,
                version: env!("CARGO_PKG_VERSION").to_string(),
                pid: Some(std::process::id()),
                uptime_secs: Some(42),
            };
            let mut w = conn;
            writeln!(w, "{}", serde_json::to_string(&pong).unwrap()).unwrap();
            w.flush().unwrap();
        });

        // Use a generous timeout: this asserts "a valid pong routes", not the
        // timeout budget (that is what the hung/trickling tests cover). Under
        // parallel `cargo test` load the server thread can be scheduled late, so
        // a tight deadline here would flake without testing anything new.
        let decision = probe_socket(&path, std::time::Duration::from_secs(5));
        assert!(
            matches!(decision, RouteDecision::Route(_)),
            "expected Route"
        );
        server.join().unwrap();
    }

    /// A present socket whose owner never answers => timeout => Direct.
    #[test]
    fn hung_service_times_out_to_direct() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("service.sock")).unwrap();
        let listener = UnixListener::bind(&path).unwrap();

        // Accept but never reply. Hold the connection until the client gives up
        // and closes (the trailing read unblocks then), so the thread exits
        // promptly without a fixed sleep that would slow the suite.
        let server = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut buf = [0u8; 64];
            let _ = conn.read(&mut buf); // the ping
            let _ = conn.read(&mut buf); // blocks until the client closes
        });

        let start = std::time::Instant::now();
        let decision = probe_socket(&path, std::time::Duration::from_millis(100));
        assert!(
            matches!(decision, RouteDecision::Direct),
            "a hung service must fall back to Direct"
        );
        // 100ms timeout vs a 1s bound: catches a probe that hangs or waits far
        // too long, while tolerating scheduler jitter under parallel test load.
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "probe must give up near the timeout, not block indefinitely (elapsed {:?})",
            start.elapsed()
        );
        server.join().unwrap();
    }

    /// A live listener that answers with the wrong protocol version => Direct.
    #[test]
    fn protocol_mismatch_is_direct() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("service.sock")).unwrap();
        let listener = UnixListener::bind(&path).unwrap();

        let server = thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut w = conn;
            // Version matches (so the protocol check, not a version-skew
            // check, is what's under test) but the protocol number doesn't.
            let pong = ControlFrame::Pong {
                protocol: 9999,
                version: env!("CARGO_PKG_VERSION").to_string(),
                pid: None,
                uptime_secs: None,
            };
            writeln!(w, "{}", serde_json::to_string(&pong).unwrap()).unwrap();
            w.flush().unwrap();
        });

        assert!(matches!(
            probe_socket(&path, DEFAULT_HANDSHAKE_TIMEOUT),
            RouteDecision::Direct
        ));
        server.join().unwrap();
    }

    /// A live listener that answers with the right protocol but a different
    /// build version => Direct at the `probe_socket` level, and specifically
    /// `HandshakeError::VersionSkew` (carrying both versions) at the
    /// `handshake` level — the case the client-side stderr line is keyed on.
    #[test]
    fn version_skew_is_direct() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("service.sock")).unwrap();
        let listener = UnixListener::bind(&path).unwrap();

        let server = thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut w = conn;
            let pong = ControlFrame::Pong {
                protocol: CONTROL_PROTOCOL,
                version: "0.0.1".to_string(),
                pid: Some(1),
                uptime_secs: Some(1),
            };
            writeln!(w, "{}", serde_json::to_string(&pong).unwrap()).unwrap();
            w.flush().unwrap();
        });

        assert!(matches!(
            probe_socket(&path, DEFAULT_HANDSHAKE_TIMEOUT),
            RouteDecision::Direct
        ));
        server.join().unwrap();
    }

    /// Unit-test `handshake` directly (rather than through `probe_socket`) to
    /// assert the `VersionSkew` variant itself carries both the server's and
    /// this client's version strings — the detail `probe_socket` discards
    /// after printing (its result is just `RouteDecision`).
    #[test]
    fn handshake_reports_version_skew_with_both_versions() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("service.sock")).unwrap();
        let listener = UnixListener::bind(&path).unwrap();

        let server = thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut w = conn;
            let pong = ControlFrame::Pong {
                protocol: CONTROL_PROTOCOL,
                version: "0.0.1".to_string(),
                pid: None,
                uptime_secs: None,
            };
            writeln!(w, "{}", serde_json::to_string(&pong).unwrap()).unwrap();
            w.flush().unwrap();
        });

        let stream = connect_control(&path, DEFAULT_HANDSHAKE_TIMEOUT).unwrap();
        let err = handshake(&stream, DEFAULT_HANDSHAKE_TIMEOUT).expect_err("expected VersionSkew");
        match err {
            HandshakeError::VersionSkew { server, client } => {
                assert_eq!(server, "0.0.1");
                assert_eq!(client, env!("CARGO_PKG_VERSION"));
            }
            HandshakeError::Other(e) => panic!("expected VersionSkew, got Other({e:?})"),
        }
        server.join().unwrap();
    }

    /// FIX-8 (NRN-93): the version is compared BEFORE the protocol
    /// short-circuit, so a well-formed pong at a *different* version but a
    /// future/other protocol is a `VersionSkew` (actionable stderr note), not a
    /// silent `Other`. Guards against a future `CONTROL_PROTOCOL` bump silently
    /// masking version skew and mapping an old daemon to Direct with no signal.
    #[test]
    fn version_skew_beats_protocol_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("service.sock")).unwrap();
        let listener = UnixListener::bind(&path).unwrap();

        let server = thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut w = conn;
            // Different protocol AND different version.
            let pong = ControlFrame::Pong {
                protocol: 9999,
                version: "0.0.1".to_string(),
                pid: None,
                uptime_secs: None,
            };
            writeln!(w, "{}", serde_json::to_string(&pong).unwrap()).unwrap();
            w.flush().unwrap();
        });

        let stream = connect_control(&path, DEFAULT_HANDSHAKE_TIMEOUT).unwrap();
        let err = handshake(&stream, DEFAULT_HANDSHAKE_TIMEOUT)
            .expect_err("expected VersionSkew, not Other");
        assert!(
            matches!(err, HandshakeError::VersionSkew { .. }),
            "protocol 9999 + version 0.0.1 must be VersionSkew (version compared first), got {err:?}"
        );
        server.join().unwrap();
    }

    /// The complementary case to `version_skew_beats_protocol_mismatch`: a
    /// pong at the SAME version as this build but a mismatched/future
    /// protocol must be silent `Other` -> Direct, NOT `VersionSkew`. A
    /// version match must not be treated as license to skip the protocol
    /// check; only a version MISMATCH is the actionable, stderr-worthy case.
    /// Asserts both ends of the contract: `probe_socket` -> `Direct`, and the
    /// underlying `handshake` error is specifically `Other`.
    #[test]
    fn matching_version_mismatched_protocol_is_silent_direct() {
        fn stub_pong_server(path: &Utf8PathBuf) -> thread::JoinHandle<()> {
            let listener = UnixListener::bind(path).unwrap();
            thread::spawn(move || {
                let (conn, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(conn.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut w = conn;
                // Same version as this build, but a mismatched protocol.
                let pong = ControlFrame::Pong {
                    protocol: 9999,
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    pid: None,
                    uptime_secs: None,
                };
                writeln!(w, "{}", serde_json::to_string(&pong).unwrap()).unwrap();
                w.flush().unwrap();
            })
        }

        // probe_socket() must fall back to Direct.
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("service.sock")).unwrap();
        let server = stub_pong_server(&path);
        assert!(
            matches!(
                probe_socket(&path, DEFAULT_HANDSHAKE_TIMEOUT),
                RouteDecision::Direct
            ),
            "matching version + mismatched protocol must still route to Direct"
        );
        server.join().unwrap();

        // The underlying handshake() error must be Other, not VersionSkew.
        let dir2 = tempfile::tempdir().unwrap();
        let path2 = Utf8PathBuf::from_path_buf(dir2.path().join("service.sock")).unwrap();
        let server2 = stub_pong_server(&path2);
        let stream = connect_control(&path2, DEFAULT_HANDSHAKE_TIMEOUT).unwrap();
        let err = handshake(&stream, DEFAULT_HANDSHAKE_TIMEOUT)
            .expect_err("mismatched protocol must not route");
        assert!(
            matches!(err, HandshakeError::Other(_)),
            "matching version + mismatched protocol must be Other, not VersionSkew: {err:?}"
        );
        server2.join().unwrap();
    }

    /// A well-formed pong missing the `version` field entirely is a
    /// malformed frame => Direct, and specifically NOT `VersionSkew` (there
    /// is no server version to report).
    #[test]
    fn pong_missing_version_is_direct_not_version_skew() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("service.sock")).unwrap();
        let listener = UnixListener::bind(&path).unwrap();

        let server = thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut w = conn;
            // Hand-written: no `version` field at all.
            writeln!(
                w,
                "{{\"norn_control\":\"pong\",\"protocol\":{CONTROL_PROTOCOL}}}"
            )
            .unwrap();
            w.flush().unwrap();
        });

        let stream = connect_control(&path, DEFAULT_HANDSHAKE_TIMEOUT).unwrap();
        let err =
            handshake(&stream, DEFAULT_HANDSHAKE_TIMEOUT).expect_err("expected a handshake error");
        assert!(
            matches!(err, HandshakeError::Other(_)),
            "a pong missing `version` must be Other, not VersionSkew: {err:?}"
        );
        server.join().unwrap();
    }

    /// Each control-frame variant serializes to exactly the documented wire
    /// shape — the newline-delimited JSON contract shared by client and
    /// daemon. Asserted via `serde_json::Value` equality so field order in
    /// the struct literal doesn't matter, only the resulting JSON.
    #[test]
    fn control_frame_wire_shapes() {
        let ping = ControlFrame::Ping {
            protocol: CONTROL_PROTOCOL,
        };
        assert_eq!(
            serde_json::to_value(&ping).unwrap(),
            serde_json::json!({"norn_control": "ping", "protocol": 1})
        );

        let pong = ControlFrame::Pong {
            protocol: CONTROL_PROTOCOL,
            version: "1.2.3".to_string(),
            pid: Some(4321),
            uptime_secs: Some(99),
        };
        assert_eq!(
            serde_json::to_value(&pong).unwrap(),
            serde_json::json!({
                "norn_control": "pong",
                "protocol": 1,
                "version": "1.2.3",
                "pid": 4321,
                "uptime_secs": 99,
            })
        );

        let hello = ControlFrame::Hello {
            protocol: CONTROL_PROTOCOL,
            vault_root: "/vaults/atlas".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&hello).unwrap(),
            serde_json::json!({
                "norn_control": "hello",
                "protocol": 1,
                "vault_root": "/vaults/atlas",
            })
        );

        let ready = ControlFrame::Ready {
            protocol: CONTROL_PROTOCOL,
            version: "1.2.3".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&ready).unwrap(),
            serde_json::json!({
                "norn_control": "ready",
                "protocol": 1,
                "version": "1.2.3",
            })
        );

        let error = ControlFrame::Error {
            protocol: CONTROL_PROTOCOL,
            message: "vault not found".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&error).unwrap(),
            serde_json::json!({
                "norn_control": "error",
                "protocol": 1,
                "message": "vault not found",
            })
        );
    }

    /// A live listener that closes without answering => Direct.
    #[test]
    fn immediate_close_is_direct() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("service.sock")).unwrap();
        let listener = UnixListener::bind(&path).unwrap();

        let server = thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            drop(conn); // close immediately, before any reply
        });

        assert!(matches!(
            probe_socket(&path, DEFAULT_HANDSHAKE_TIMEOUT),
            RouteDecision::Direct
        ));
        server.join().unwrap();
    }

    /// A peer that dribbles bytes without ever sending a newline must be bounded
    /// by the *cumulative* timeout, not per-read — otherwise `read_line`-style
    /// reads loop forever (hang + unbounded memory). Regression for the finding
    /// that `SO_RCVTIMEO` bounds each syscall, not the whole handshake.
    #[test]
    fn trickling_service_without_newline_times_out_to_direct() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("service.sock")).unwrap();
        let listener = UnixListener::bind(&path).unwrap();

        // Send one byte every 25ms, no newline, for up to ~5s (breaking once the
        // client gives up and the write fails). Each byte lands within any
        // per-read timeout, so only a cumulative bound stops the probe. The
        // pre-fix `read_line` would loop for the full ~5s (or hang); the fix
        // bails at the 120ms deadline.
        let server = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut ping = [0u8; 64];
            let _ = conn.read(&mut ping);
            for _ in 0..200 {
                if conn.write_all(b"x").is_err() || conn.flush().is_err() {
                    break;
                }
                thread::sleep(std::time::Duration::from_millis(25));
            }
        });

        let start = std::time::Instant::now();
        let decision = probe_socket(&path, std::time::Duration::from_millis(120));
        assert!(
            matches!(decision, RouteDecision::Direct),
            "a trickling never-newline service must fall back to Direct"
        );
        // 120ms timeout vs a 2s bound: the unbounded bug reads for ~5s, the fix
        // bails ~120ms; 2s cleanly separates them while tolerating load jitter.
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "probe must give up near the cumulative timeout, not read forever \
             (elapsed {:?})",
            start.elapsed()
        );
        server.join().unwrap();
    }

    /// A socket path longer than the platform `sun_path` limit (~104 on macOS)
    /// must be rejected up front rather than truncated — the connect attempt
    /// returns an error, so the probe falls back to Direct. Guards the daemon
    /// bind-path concern from the client side.
    #[test]
    fn over_length_socket_path_is_rejected_by_connect() {
        let dir = tempfile::tempdir().unwrap();
        // 200-char filename => path well over any sun_path limit.
        let long_name = "s".repeat(200);
        let path = Utf8PathBuf::from_path_buf(dir.path().join(long_name)).unwrap();
        assert!(
            connect_control(&path, DEFAULT_HANDSHAKE_TIMEOUT).is_err(),
            "an over-length socket path must be rejected, not truncated"
        );
    }

    /// A wedged daemon whose accept backlog is full must not hang the probe:
    /// the bounded connect gives up at its budget and the probe returns Direct.
    ///
    /// Setup: bind a listener by hand with the minimal backlog (`listen(fd, 1)`)
    /// and NEVER accept, then saturate the backlog with idle client
    /// connections. Platform behavior then diverges — on Linux a further
    /// `connect(2)` *blocks* until a slot frees (the finding-F2 hang: a plain
    /// blocking connect never returns), while macOS *refuses* a full backlog
    /// outright. Either way the bounded connect resolves quickly to Direct
    /// (Linux: `poll` times out at the 150ms budget; macOS: ECONNREFUSED).
    ///
    /// TEETH: this test bites on Linux — against the pre-fix blocking connect it
    /// would hang far past the 2s bound. On macOS it passes trivially because
    /// macOS never blocks on a full backlog (verified empirically), so there the
    /// assertion only guards against a regression that reintroduces blocking.
    #[test]
    fn full_backlog_connect_times_out_to_direct() {
        use std::os::fd::{FromRawFd, OwnedFd};

        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("service.sock")).unwrap();

        // Build the sockaddr_un for `path` once; reused for bind and connects.
        // SAFETY: `sockaddr_un` is POD; zeroed is a valid initialized value.
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let pb = path.as_str().as_bytes();
        assert!(
            pb.len() < addr.sun_path.len(),
            "test path must fit sun_path"
        );
        for (d, &s) in addr.sun_path.iter_mut().zip(pb) {
            *d = s as libc::c_char;
        }
        let addr_len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;

        // Bind + listen(1) by hand so the accept backlog is minimal and cheap
        // to saturate. Hold the listener fd open (never accept) for the probe.
        // SAFETY: socket/bind/listen with valid constants and a valid addr.
        let listener = unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
            assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());
            let owned = OwnedFd::from_raw_fd(fd);
            let rc = libc::bind(
                fd,
                &addr as *const libc::sockaddr_un as *const libc::sockaddr,
                addr_len,
            );
            assert_eq!(rc, 0, "bind: {}", std::io::Error::last_os_error());
            let rc = libc::listen(fd, 1);
            assert_eq!(rc, 0, "listen: {}", std::io::Error::last_os_error());
            owned
        };

        // Saturate the backlog with idle nonblocking clients; keep their fds
        // open so the slots stay occupied. Nothing ever accepts them.
        let mut fillers: Vec<OwnedFd> = Vec::new();
        for _ in 0..64 {
            // SAFETY: socket + nonblocking connect with the same valid addr;
            // the fd is owned immediately so it is closed on drop.
            unsafe {
                let cfd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
                if cfd < 0 {
                    break;
                }
                let owned = OwnedFd::from_raw_fd(cfd);
                let fl = libc::fcntl(cfd, libc::F_GETFL);
                libc::fcntl(cfd, libc::F_SETFL, fl | libc::O_NONBLOCK);
                let _ = libc::connect(
                    cfd,
                    &addr as *const libc::sockaddr_un as *const libc::sockaddr,
                    addr_len,
                );
                fillers.push(owned);
            }
        }

        let start = std::time::Instant::now();
        let decision = probe_socket(&path, std::time::Duration::from_millis(150));
        assert!(
            matches!(decision, RouteDecision::Direct),
            "a full-backlog (wedged) service must fall back to Direct"
        );
        // 150ms budget vs a 2s bound: the pre-fix blocking connect hangs on
        // Linux; the bounded connect gives up ~150ms. 2s separates them with
        // ample room for load jitter.
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "probe must give up near the connect budget, not block (elapsed {:?})",
            start.elapsed()
        );

        // Keep the sockets alive until here so the backlog stayed full during
        // the probe; drop explicitly to make that lifetime intent clear.
        drop(fillers);
        drop(listener);
    }

    /// Spawn a stub listener that answers the first line with `pong_json`.
    /// Returns the socket path (in `dir`) and the server join handle.
    fn pong_stub(
        dir: &tempfile::TempDir,
        pong_json: serde_json::Value,
    ) -> (Utf8PathBuf, thread::JoinHandle<()>) {
        let path = Utf8PathBuf::from_path_buf(dir.path().join("service.sock")).unwrap();
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut w = conn;
            writeln!(w, "{pong_json}").unwrap();
            w.flush().unwrap();
        });
        (path, server)
    }

    /// NRN-115 status probe: a healthy pong's version/pid/uptime come through —
    /// including a pong at a DIFFERENT build version, because status's job is
    /// to report that skew as restart-pending rather than hide the daemon.
    #[test]
    fn probe_status_keeps_pong_fields_and_ignores_version() {
        let dir = tempfile::tempdir().unwrap();
        let (path, server) = pong_stub(
            &dir,
            serde_json::json!({
                "norn_control": "pong",
                "protocol": CONTROL_PROTOCOL,
                "version": "0.0.1-stale",
                "pid": 777,
                "uptime_secs": 42,
            }),
        );
        let pong = probe_status_socket(&path, DEFAULT_HANDSHAKE_TIMEOUT)
            .expect("a protocol-matching pong must come through regardless of version");
        assert_eq!(pong.version, "0.0.1-stale");
        assert_eq!(pong.pid, Some(777));
        assert_eq!(pong.uptime_secs, Some(42));
        server.join().unwrap();
    }

    /// NRN-115 review: the status probe must respect the PROTOCOL gate. A pong
    /// at a foreign control protocol — even at the SAME build version — is not
    /// a healthy daemon and must render as no-answer (None), never as healthy
    /// running state assembled from a wire we don't speak.
    #[test]
    fn probe_status_rejects_a_protocol_mismatched_pong() {
        let dir = tempfile::tempdir().unwrap();
        let (path, server) = pong_stub(
            &dir,
            serde_json::json!({
                "norn_control": "pong",
                "protocol": CONTROL_PROTOCOL + 999,
                "version": env!("CARGO_PKG_VERSION"),
                "pid": 777,
                "uptime_secs": 42,
            }),
        );
        assert!(
            probe_status_socket(&path, DEFAULT_HANDSHAKE_TIMEOUT).is_none(),
            "a same-version pong at a foreign protocol must be None (unhealthy)"
        );
        server.join().unwrap();
    }

    /// No socket file: the status probe is None without connecting.
    #[test]
    fn probe_status_no_socket_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("absent.sock")).unwrap();
        assert!(probe_status_socket(&path, DEFAULT_HANDSHAKE_TIMEOUT).is_none());
    }
}
