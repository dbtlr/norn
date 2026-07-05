//! End-to-end lifecycle coverage for the `norn serve` host daemon (NRN-93).
//!
//! Each test spawns its own daemon on a private `XDG_CACHE_HOME`, so the
//! well-known socket is isolated per test (see `serve_util`). Readiness is a
//! deadline-loop ping, never a fixed sleep; every child is reaped by a drop
//! guard. Covers: identity in the pong, single-owner refusal, clean SIGTERM
//! shutdown (socket unlinked), and stale-socket reclaim after a crash.

#[path = "serve_util/mod.rs"]
mod serve_util;

use std::io::Read as _;
use std::process::Stdio;
use std::time::Duration;

use serve_util::*;

/// A freshly started daemon answers a control ping with a pong that reports this
/// build's version, the daemon's pid, and an uptime.
#[test]
fn starts_answers_ping_and_reports_identity() {
    let daemon = spawn_ready_daemon();

    let pong = try_ping(&daemon.socket_path).expect("daemon must answer a ping");
    assert_eq!(pong["norn_control"], "pong", "expected a pong: {pong}");
    assert_eq!(pong["protocol"], 1, "protocol must be 1: {pong}");
    assert_eq!(
        pong["version"].as_str(),
        Some(env!("CARGO_PKG_VERSION")),
        "pong version must match this build: {pong}"
    );
    assert_eq!(
        pong["pid"].as_u64(),
        Some(u64::from(daemon.pid())),
        "pong pid must be the daemon child's pid: {pong}"
    );
    assert!(
        pong["uptime_secs"].is_u64(),
        "pong must report uptime_secs: {pong}"
    );
}

/// With one daemon live, a second `norn serve` on the SAME cache home refuses to
/// start: it exits non-zero promptly and says it is already running.
#[test]
fn second_instance_refuses() {
    let daemon = spawn_ready_daemon();

    // Second instance shares the first's run dir (same XDG_CACHE_HOME); it must
    // fail the single-owner flock. Capture its stderr for the message assertion.
    let mut second = build_serve_command(&daemon.cache_home, &daemon.state_home)
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn second norn serve");

    let status = wait_for_exit(&mut second, Duration::from_secs(10))
        .expect("second instance must exit promptly, not hang");
    assert!(
        !status.success(),
        "second instance must exit non-zero, got {status}"
    );

    let mut stderr = String::new();
    second
        .stderr
        .take()
        .expect("captured stderr")
        .read_to_string(&mut stderr)
        .expect("read second-instance stderr");
    assert!(
        stderr.contains("already running"),
        "second-instance stderr must mention it is already running, got: {stderr:?}"
    );
}

/// SIGTERM shuts the daemon down cleanly: it exits 0 and unlinks its socket.
#[test]
fn sigterm_unlinks_socket() {
    let mut daemon = spawn_ready_daemon();
    assert!(
        daemon.socket_path.exists(),
        "socket must exist while the daemon runs"
    );

    send_signal(daemon.pid(), libc::SIGTERM);

    let status = wait_for_exit(&mut daemon.child.0, Duration::from_secs(10))
        .expect("daemon must exit after SIGTERM, not hang");
    assert!(
        status.success(),
        "clean SIGTERM shutdown must exit 0, got {status}"
    );
    assert!(
        !daemon.socket_path.exists(),
        "the daemon must unlink its socket on SIGTERM shutdown"
    );
}

/// A SIGKILL'd daemon leaves its socket file behind (no cleanup ran); a fresh
/// daemon on the same run dir reclaims that stale socket and serves normally.
#[test]
fn stale_socket_reclaimed() {
    let mut first = spawn_ready_daemon();
    let cache_home = first.cache_home.clone();
    let state_home = first.state_home.clone();
    let socket_path = first.socket_path.clone();

    // SIGKILL: no signal handler runs, so the socket file survives. The OS
    // releases the advisory flock on process death, freeing single ownership.
    send_signal(first.pid(), libc::SIGKILL);
    let status = wait_for_exit(&mut first.child.0, Duration::from_secs(10))
        .expect("killed daemon must be reaped");
    assert!(!status.success(), "a SIGKILL'd process is not a clean exit");
    assert!(
        socket_path.exists(),
        "SIGKILL must leave the stale socket file behind (no cleanup ran)"
    );

    // Fresh daemon on the same run dir: it must reclaim the stale socket, bind,
    // and answer a ping. Keep `first` alive so its tempdir (the shared cache
    // home) is not removed out from under the replacement.
    let second = ChildGuard(
        build_serve_command(&cache_home, &state_home)
            .spawn()
            .expect("spawn replacement daemon"),
    );
    wait_for_ready(&socket_path, Duration::from_secs(10));
    let pong = try_ping(&socket_path).expect("replacement daemon must answer a ping");
    assert_eq!(pong["norn_control"], "pong", "expected a pong: {pong}");
    assert_eq!(
        pong["pid"].as_u64(),
        Some(u64::from(second.pid())),
        "the ping must be answered by the REPLACEMENT daemon: {pong}"
    );

    drop(second);
    drop(first);
}
