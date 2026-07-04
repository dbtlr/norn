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
///
/// `Route` carries the live, already-handshaked connection so the caller
/// (NRN-94) can send the real request on it without a second connect.
pub enum RouteDecision {
    /// No live service — run the operation directly with a verified cache open.
    Direct,
    /// A live service answered the handshake; route the request to it.
    #[cfg(unix)]
    Route(ServiceClient),
}

/// A validated, live connection to a vault's warm service.
///
/// Holding this is proof that a daemon answered the control handshake for
/// *this* request. NRN-94 will translate the CLI args to the MCP tool contract
/// and exchange them over this connection.
#[cfg(unix)]
// Fields are written by the probe (proof of a live handshake) and read by
// NRN-94's routing implementation, which does not exist yet. Allow until then
// rather than dropping fields the very next task needs.
#[allow(dead_code)]
pub struct ServiceClient {
    /// The connection that carried a successful handshake.
    pub stream: std::os::unix::net::UnixStream,
    /// The socket path it connected to (for diagnostics / reconnection).
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

/// Probe a specific socket path. Split from [`probe`] so tests can point it at a
/// stub listener on a temp path.
#[cfg(unix)]
pub fn probe_socket(socket_path: &Utf8Path, timeout: std::time::Duration) -> RouteDecision {
    use std::os::unix::net::UnixStream;

    // Fast path: no socket file => no daemon. Just a stat; the common case pays
    // nothing beyond it. `exists()` follows symlinks and swallows errors as
    // false, which is the behavior we want (treat any doubt as "no service").
    if !socket_path.exists() {
        return RouteDecision::Direct;
    }

    // A socket file is present. Connect — a stale/orphaned socket (daemon died
    // without unlinking) refuses the connection, which we treat as Direct.
    let Ok(stream) = UnixStream::connect(socket_path) else {
        return RouteDecision::Direct;
    };

    match handshake(&stream, timeout) {
        Ok(()) => RouteDecision::Route(ServiceClient {
            stream,
            socket_path: socket_path.to_owned(),
        }),
        // Hung (timeout), refused mid-handshake, version skew, or garbage — all
        // fall back to a verified direct open. Trust is never skipped.
        Err(_) => RouteDecision::Direct,
    }
}

/// Non-Unix stub: no UDS, so routing is never available; always run Direct.
#[cfg(not(unix))]
pub fn probe_socket(_socket_path: &Utf8Path, _timeout: std::time::Duration) -> RouteDecision {
    RouteDecision::Direct
}

/// Exchange the control ping/pong on a connected stream within `timeout`.
///
/// Writes a single newline-delimited JSON ping, then reads one line and
/// requires a matching-version pong. Any I/O error, timeout, protocol-version
/// mismatch, or unexpected frame is an `Err`, which the caller maps to Direct.
#[cfg(unix)]
fn handshake(
    stream: &std::os::unix::net::UnixStream,
    timeout: std::time::Duration,
) -> anyhow::Result<()> {
    use std::io::{BufRead, BufReader, Write};

    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    // Send the ping.
    let ping = serde_json::json!({ "norn_control": "ping", "protocol": CONTROL_PROTOCOL });
    let mut writer = stream.try_clone()?;
    writeln!(writer, "{ping}")?;
    writer.flush()?;

    // Read exactly one line of response, bounded by the read timeout.
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        anyhow::bail!("service closed the connection before answering the handshake");
    }

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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
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

        let decision = probe_socket(&path, DEFAULT_HANDSHAKE_TIMEOUT);
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

        // Accept but never reply, holding the connection open.
        let server = thread::spawn(move || {
            let (_conn, _) = listener.accept().unwrap();
            thread::sleep(std::time::Duration::from_millis(500));
        });

        let start = std::time::Instant::now();
        let decision = probe_socket(&path, std::time::Duration::from_millis(100));
        assert!(
            matches!(decision, RouteDecision::Direct),
            "a hung service must fall back to Direct"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_millis(450),
            "probe must give up near the timeout, not wait for the server"
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
}
