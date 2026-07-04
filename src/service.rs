//! The CLI → service routing seam (NRN-92).
//!
//! On each invocation the CLI decides, for a routable read command, whether a
//! warm [`norn-service`] daemon is live for this vault and can serve the request
//! from an already-verified warm cache — or whether to run the operation
//! **directly** with its own integrity-verified cache open (today's behavior).
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
//! ## Scope of this module (NRN-92)
//!
//! This lands the *client half*: socket-path derivation, the probe, and the
//! control-frame protocol. It returns a [`RouteDecision`]; the daemon that
//! answers the handshake is NRN-93, and translating the CLI args to the MCP
//! contract and rendering the routed response is NRN-94. Until NRN-94 fills the
//! `Route` arm, callers treat a live service as a safe fall-through to Direct —
//! a verified direct open always preserves the trust invariant.
//!
//! Unix-domain sockets are Unix-only; on non-Unix targets [`probe`] is a
//! compile-time `Direct`, so the crate still builds everywhere.

use camino::{Utf8Path, Utf8PathBuf};

use crate::cache::cache_dir_for;

/// Control-frame protocol version. Bumped only on a breaking change to the
/// handshake wire shape; the client refuses to route unless the daemon echoes
/// the same version, so a version skew falls back to Direct rather than
/// misinterpreting frames.
pub const CONTROL_PROTOCOL: u32 = 1;

/// Filename of the per-vault service socket, under the vault's cache dir.
const SOCKET_FILENAME: &str = "service.sock";

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

/// Derive the per-vault service socket path: `<cache_dir>/service.sock`, where
/// the cache dir is `<XDG_CACHE_HOME>/norn/<sha256-of-canonical-root>/`.
///
/// Routing is by **derivation, not a registry**: both the client and the daemon
/// compute this same path from the vault root's identity hash, so N vaults on a
/// host get N independent single-owner daemons with no central coordinator.
pub fn service_socket_path(vault_root: &Utf8Path) -> anyhow::Result<Utf8PathBuf> {
    let (_canonical, cache_dir) = cache_dir_for(vault_root)?;
    Ok(cache_dir.join(SOCKET_FILENAME))
}

/// Probe for a live service for `vault_root` and decide how to run the request.
///
/// Never errors: any failure to derive the path, stat, connect, or handshake
/// resolves to [`RouteDecision::Direct`] — the always-safe path. The daemon is
/// a pure optimization, so its absence or malfunction must degrade to today's
/// behavior, never surface as an error.
pub fn probe(vault_root: &Utf8Path, timeout: std::time::Duration) -> RouteDecision {
    let Ok(socket_path) = service_socket_path(vault_root) else {
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
        // Hung (timeout), refused mid-handshake, version skew, or garbage — all
        // fall back to a verified direct open. Trust is never skipped.
        Err(_) => RouteDecision::Direct,
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
/// Writes a single newline-delimited JSON ping, then reads one line and
/// requires a matching-version pong. Any I/O error, timeout, protocol-version
/// mismatch, or unexpected frame is an `Err`, which the caller maps to Direct.
///
/// The stream is used only to prove liveness and is dropped by the caller
/// afterward, so the short handshake timeouts never leak onto a request channel.
#[cfg(unix)]
fn handshake(
    stream: &std::os::unix::net::UnixStream,
    timeout: std::time::Duration,
) -> anyhow::Result<()> {
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
    let ping = serde_json::json!({ "norn_control": "ping", "protocol": CONTROL_PROTOCOL });
    writeln!(&mut { stream }, "{ping}")?;
    Write::flush(&mut { stream })?;

    // Read one control line, bounded by the *cumulative* deadline (not per read
    // syscall) and by a max byte cap, so a trickle of bytes without a newline
    // cannot hang the probe or grow memory unbounded.
    let line = read_control_line(stream, deadline)?;

    let frame: serde_json::Value = serde_json::from_str(line.trim())?;
    let kind = frame.get("norn_control").and_then(|v| v.as_str());
    let protocol = frame.get("protocol").and_then(|v| v.as_u64());
    if kind != Some("pong") {
        anyhow::bail!("unexpected control frame: {kind:?}");
    }
    if protocol != Some(u64::from(CONTROL_PROTOCOL)) {
        anyhow::bail!("control protocol mismatch: service spoke {protocol:?}, client wants {CONTROL_PROTOCOL}");
    }

    Ok(())
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

    /// Derivation is deterministic and lands under the vault's cache dir.
    /// The roots must exist because `vault_identity` canonicalizes them.
    #[test]
    fn socket_path_is_deterministic_and_under_cache_dir() {
        let vault = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(vault.path()).unwrap();
        let a = service_socket_path(root).expect("derive");
        let b = service_socket_path(root).expect("derive again");
        assert_eq!(a, b, "same vault => same socket path");
        assert!(a.as_str().ends_with("/service.sock"), "path was {a}");
        assert!(
            a.as_str().contains("/norn/"),
            "under the norn cache tree: {a}"
        );

        let other_vault = tempfile::tempdir().unwrap();
        let other_root = Utf8Path::from_path(other_vault.path()).unwrap();
        let other = service_socket_path(other_root).expect("derive other");
        assert_ne!(a, other, "distinct vaults => distinct sockets");
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

    /// A live listener that answers a prompt, well-formed pong => Route.
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
            // Answer with a matching-version pong.
            let mut w = conn;
            writeln!(
                w,
                "{{\"norn_control\":\"pong\",\"protocol\":{CONTROL_PROTOCOL}}}"
            )
            .unwrap();
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
            writeln!(w, "{{\"norn_control\":\"pong\",\"protocol\":9999}}").unwrap();
            w.flush().unwrap();
        });

        assert!(matches!(
            probe_socket(&path, DEFAULT_HANDSHAKE_TIMEOUT),
            RouteDecision::Direct
        ));
        server.join().unwrap();
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
