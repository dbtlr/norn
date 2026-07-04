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
//! This lands the *client half*: host-socket-path derivation, the probe, and
//! the control-frame protocol. It returns a [`RouteDecision`]; the daemon that
//! answers the handshake is NRN-93, and translating the CLI args to the MCP
//! contract and rendering the routed response is NRN-94. Until NRN-94 fills the
//! `Route` arm, callers treat a live service as a safe fall-through to Direct —
//! a verified direct open always preserves the trust invariant.
//!
//! Unix-domain sockets are Unix-only; on non-Unix targets [`probe`] is a
//! compile-time `Direct`, so the crate still builds everywhere.

use camino::{Utf8Path, Utf8PathBuf};

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
    #[allow(dead_code)]
    Hello { protocol: u32, vault_root: String },
    /// Daemon -> client: the named vault's warm cache is ready to serve.
    /// Unused until NRN-94.
    #[allow(dead_code)]
    Ready { protocol: u32, version: String },
    /// Daemon -> client: the request (handshake or named-vault open) failed
    /// on the daemon side. Unused until NRN-94.
    #[allow(dead_code)]
    Error { protocol: u32, message: String },
}

/// A handshake outcome that distinguishes version skew from every other
/// failure mode.
///
/// Every handshake failure maps to [`RouteDecision::Direct`] — trust is never
/// skipped just because routing didn't happen — but version skew is the one
/// failure worth telling the operator about: it is actionable (the fix is
/// `norn service restart`) and silent about it would leave the CLI quietly
/// falling back to Direct forever after a client upgrade, with no signal that
/// a stale daemon is the reason. Every other variant (`Other`) covers
/// timeouts, I/O errors, protocol mismatches, and malformed frames, which
/// stay silent because they are expected transient or environmental noise.
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

/// The routing decision for one CLI invocation.
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
// `socket_path` is written by the probe and consumed by NRN-94's routing
// implementation, which does not exist yet. Allow until then rather than
// dropping the field the very next task needs.
#[allow(dead_code)]
pub struct ServiceClient {
    /// The socket a handshake just succeeded against — where NRN-94 connects.
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
/// Unused by the client today — the daemon lifecycle that acquires this lock
/// on startup is a later task (NRN-93's daemon binary). Kept `pub` and
/// defined here now so the run-directory layout (socket + lock, one place)
/// is settled by the same change that fixes the socket path.
#[allow(dead_code)]
pub fn host_lock_path() -> anyhow::Result<Utf8PathBuf> {
    Ok(run_dir()?.join("norn.lock"))
}

/// Probe for a live host service and decide how to run the request.
///
/// Never errors: any failure to derive the path, stat, connect, or handshake
/// resolves to [`RouteDecision::Direct`] — the always-safe path. The daemon is
/// a pure optimization, so its absence or malfunction must degrade to today's
/// behavior, never surface as an error.
pub fn probe(timeout: std::time::Duration) -> RouteDecision {
    let Ok(socket_path) = host_socket_path() else {
        return RouteDecision::Direct;
    };
    probe_socket(&socket_path, timeout)
}

/// Upper bound on the control-frame response size. The pong is a few dozen
/// bytes; this cap turns a peer that streams bytes without a newline into a
/// bounded `Err` (→ Direct) instead of an unbounded buffer growth.
#[cfg(unix)]
const MAX_CONTROL_FRAME_BYTES: usize = 8 * 1024;

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

    // A socket file is present. Connect — a stale/orphaned socket (daemon died
    // without unlinking) refuses, and an over-length path (macOS `sun_path` is
    // ~104 bytes) is rejected by the OS; both map to Direct.
    //
    // NOTE: `connect` itself has no timeout. A pathological wedged daemon whose
    // accept backlog is full could block it. That path is unreachable until a
    // daemon exists (NRN-93 binds the socket), so bounding the *connect* is
    // deferred to NRN-93 alongside the daemon lifecycle it belongs with; the
    // handshake read below is already deadline-bounded.
    let Ok(stream) = connect_control(socket_path) else {
        return RouteDecision::Direct;
    };

    let decision = match handshake(&stream, timeout) {
        Ok(()) => RouteDecision::Route(ServiceClient {
            socket_path: socket_path.to_owned(),
        }),
        // Version skew is the one failure mode worth telling the operator
        // about — it's actionable and otherwise invisible (the CLI would
        // just quietly run Direct forever after a client upgrade). Exactly
        // one line, then fall back like every other failure.
        Err(HandshakeError::VersionSkew { server, client }) => {
            eprintln!(
                "norn: service is v{server}, client is v{client} — run 'norn service restart'"
            );
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
pub fn probe_socket(_socket_path: &Utf8Path, _timeout: std::time::Duration) -> RouteDecision {
    RouteDecision::Direct
}

/// Connect to the control socket. Blocking `std` connect: it returns an error
/// for a refused (stale/orphaned) socket and for an over-length path (the OS
/// rejects a `sun_path` beyond ~104 bytes rather than truncating), both of which
/// the caller maps to Direct. It has no *connect* timeout — see the note at the
/// call site; the handshake read that follows is deadline-bounded.
#[cfg(unix)]
fn connect_control(socket_path: &Utf8Path) -> std::io::Result<std::os::unix::net::UnixStream> {
    std::os::unix::net::UnixStream::connect(socket_path.as_std_path())
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
    let version = handshake_pong_version(stream, timeout).map_err(HandshakeError::Other)?;
    let client_version = env!("CARGO_PKG_VERSION");
    if version != client_version {
        return Err(HandshakeError::VersionSkew {
            server: version,
            client: client_version.to_string(),
        });
    }
    Ok(())
}

/// Write the ping, read the reply, and return the pong's `version` string —
/// the piece [`handshake`] needs to decide routing vs. version skew. Every
/// failure short of a well-formed, protocol-matching pong is a plain
/// `anyhow::Error`; [`handshake`] is the only place that turns "the version
/// differs" into the distinguished [`HandshakeError::VersionSkew`].
#[cfg(unix)]
fn handshake_pong_version(
    stream: &std::os::unix::net::UnixStream,
    timeout: std::time::Duration,
) -> anyhow::Result<String> {
    use std::io::Write;

    let deadline = std::time::Instant::now() + timeout;
    // Set the I/O timeouts once. `SO_RCVTIMEO` bounds each read; the read loop's
    // wall-clock deadline check bounds the *total*, so a single set suffices —
    // and setting it once (rather than per read) avoids the repeated setsockopt
    // that spuriously trips EINVAL on macOS under heavy concurrency.
    stream.set_write_timeout(Some(timeout))?;
    stream.set_read_timeout(Some(timeout))?;

    // All I/O rides the borrowed stream directly — `&UnixStream` implements
    // `Read`/`Write`, so no `try_clone` is needed. (Cloning then dropping a
    // dup'd socket fd before touching `SO_RCVTIMEO` on the original trips EINVAL
    // on macOS; borrowing sidesteps that and avoids the fd churn.)
    let ping = ControlFrame::Ping {
        protocol: CONTROL_PROTOCOL,
    };
    let ping_json = serde_json::to_string(&ping)?;
    writeln!(&mut { stream }, "{ping_json}")?;
    Write::flush(&mut { stream })?;

    // Read one control line, bounded by the *cumulative* deadline (not per read
    // syscall) and by a max byte cap, so a trickle of bytes without a newline
    // cannot hang the probe or grow memory unbounded.
    let line = read_control_line(stream, deadline)?;

    // A pong missing `version` fails to deserialize here (it's a required
    // field) and lands in the generic `Err` path below — a malformed frame,
    // not a version skew.
    let frame: ControlFrame = serde_json::from_str(line.trim())?;
    match frame {
        ControlFrame::Pong {
            protocol, version, ..
        } => {
            if protocol != CONTROL_PROTOCOL {
                anyhow::bail!(
                    "control protocol mismatch: service spoke {protocol}, client wants {CONTROL_PROTOCOL}"
                );
            }
            Ok(version)
        }
        other => anyhow::bail!("unexpected control frame: {other:?}"),
    }
}

/// Read a single newline-terminated control frame, bounded by a wall-clock
/// `deadline` and [`MAX_CONTROL_FRAME_BYTES`].
///
/// The caller sets `SO_RCVTIMEO` (bounding each individual read); this loop's
/// deadline check bounds the *cumulative* time — the key fix over a plain
/// `read_line`, which a peer that dribbles bytes under the per-read timeout can
/// keep alive forever. Worst-case overshoot is one read's `SO_RCVTIMEO` past the
/// deadline, which is fine for a liveness probe.
#[cfg(unix)]
fn read_control_line(
    stream: &std::os::unix::net::UnixStream,
    deadline: std::time::Instant,
) -> anyhow::Result<String> {
    use std::io::Read;

    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("handshake timed out before a full control frame arrived");
        }
        let n = Read::read(&mut { stream }, &mut chunk)?;
        if n == 0 {
            anyhow::bail!("service closed the connection before answering the handshake");
        }
        if let Some(nl) = chunk[..n].iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&chunk[..nl]);
            return Ok(String::from_utf8_lossy(&buf).into_owned());
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_CONTROL_FRAME_BYTES {
            anyhow::bail!(
                "control frame exceeded {MAX_CONTROL_FRAME_BYTES} bytes without a newline"
            );
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;

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

        let stream = connect_control(&path).unwrap();
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

        let stream = connect_control(&path).unwrap();
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
            connect_control(&path).is_err(),
            "an over-length socket path must be rejected, not truncated"
        );
    }
}
