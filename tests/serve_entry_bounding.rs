//! Bounded per-vault entry retention + fail-closed self-heal for the `norn serve`
//! daemon (NRN-337).
//!
//! Two end-to-end proofs against a real daemon:
//!
//! - **Eviction bounds fds.** Driving many distinct vaults through a daemon with
//!   a tiny open-entry cap keeps the retained-context count at or below the cap
//!   (read from the `service status` pong), and a vault whose context was evicted
//!   still serves correctly on a later request (the reopen path).
//! - **Self-heal on poisoned state.** A daemon run under a low `RLIMIT_NOFILE`
//!   exhausts its fd table as distinct vaults accumulate, hits the poisoned
//!   (EMFILE) class, and EXITS to self-heal rather than serving errors forever.

#[path = "serve_util/mod.rs"]
mod serve_util;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serve_util::*;
use tempfile::TempDir;

fn note(title: &str) -> String {
    format!("---\ntype: note\ntitle: {title}\n---\n{title} body\n")
}

/// A distinct temp vault with one seeded note, kept alive by the returned handle.
fn seed_vault(i: usize) -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-evict-vault-")
        .tempdir()
        .expect("tempdir");
    std::fs::write(tmp.path().join("n.md"), note(&format!("Vault {i}"))).unwrap();
    tmp
}

/// Host-global open-entries count from a status ping's pong, or `None` if the
/// pong lacks the field (shouldn't happen against a current daemon).
fn open_entries(socket_path: &Path) -> Option<u64> {
    try_ping(socket_path)?
        .get("host")?
        .get("open_entries")?
        .as_u64()
}

/// Open a vault (hello + initialize + one `vault.find`) and immediately close the
/// connection, so the context goes idle and becomes eligible for eviction.
fn touch_vault(socket_path: &Path, vault: &Path) {
    let mut conn = connect_and_hello(socket_path, vault);
    conn.initialize();
    let resp = conn.call_tool("vault.find", serde_json::json!({ "limit": 100 }));
    assert!(resp.get("error").is_none(), "find must not error: {resp}");
    // conn dropped here → connection closes → context goes idle.
}

/// The open-entry cap is enforced (LRU): after driving many distinct vaults, the
/// retained-context count converges to at most the cap, and a vault whose context
/// was evicted still serves correctly on a later request.
#[test]
fn idle_contexts_are_evicted_to_bound_open_entries() {
    const CAP: u64 = 4;
    let daemon = spawn_ready_daemon_with_log(&[("NORN_SERVE_MAX_ENTRIES", "4")]);

    // Keep the vault dirs alive for the whole test (dropping a TempDir deletes it,
    // which would turn a later reopen into a RootGone rather than a reopen).
    let vaults: Vec<TempDir> = (0..12).map(seed_vault).collect();

    // Drive every vault once, closing each connection so its context goes idle.
    for vault in &vaults {
        touch_vault(&daemon.socket_path, vault.path());
        // Give the daemon a beat to reap the just-closed connection so the context
        // is observably idle before the next hello's eviction pass.
        std::thread::sleep(Duration::from_millis(30));
    }

    // Eviction runs at entry-open, so keep driving throwaway hellos until the
    // retained count converges to <= CAP (or fail at the deadline).
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut extra = 100usize;
    loop {
        let observed = open_entries(&daemon.socket_path)
            .expect("pong must carry host.open_entries against a current daemon");
        if observed <= CAP {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "open_entries never converged to <= {CAP}; last observed {observed}"
        );
        // Drive an extra hello to trigger another eviction pass.
        let v = seed_vault(extra);
        touch_vault(&daemon.socket_path, v.path());
        extra += 1;
        std::thread::sleep(Duration::from_millis(30));
    }

    // The count is bounded regardless of how many distinct vaults were served.
    let bounded = open_entries(&daemon.socket_path).unwrap();
    assert!(
        bounded <= CAP,
        "retained contexts must stay <= cap {CAP}, got {bounded}"
    );

    // The very first vault's context was evicted long ago; a fresh request must
    // transparently reopen it and return correct results (the reopen path).
    let mut conn = connect_and_hello(&daemon.socket_path, vaults[0].path());
    conn.initialize();
    let docs = find_documents(&conn.call_tool("vault.find", serde_json::json!({ "limit": 100 })));
    assert_eq!(
        paths(&docs),
        vec!["n.md"],
        "an evicted vault must serve correctly on reopen"
    );
}

/// Spawn a daemon under a low `RLIMIT_NOFILE` (via `pre_exec`) with the open-entry
/// cap effectively disabled, so accumulating distinct vaults exhaust the fd table
/// deterministically. Stderr is captured so the test can assert the self-heal log
/// line. Returns the child plus its stderr path and socket path; the tempdir is
/// leaked into the returned tuple to keep the socket alive.
fn spawn_fd_starved_daemon() -> (
    std::process::Child,
    std::path::PathBuf,
    std::path::PathBuf,
    TempDir,
) {
    let tmp = tempfile::Builder::new().prefix("nfd-").tempdir().unwrap();
    let cache_home = tmp.path().join("c");
    let state_home = tmp.path().join("s");
    let stderr_path = tmp.path().join("err");
    let stderr_file = std::fs::File::create(&stderr_path).unwrap();

    let mut cmd = Command::new(norn_bin());
    cmd.arg("serve")
        .env("XDG_CACHE_HOME", &cache_home)
        .env("XDG_STATE_HOME", &state_home)
        // Disable eviction so fd exhaustion (not the cap) is the trigger.
        .env("NORN_SERVE_MAX_ENTRIES", "100000")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file));
    // SAFETY: `setrlimit(2)` in the forked child before exec is async-signal-safe
    // and touches no shared state. A soft (and hard) NOFILE of 128 is comfortably
    // above daemon startup yet crossed after ~a dozen distinct vaults (~7 fds each).
    unsafe {
        cmd.pre_exec(|| {
            let lim = libc::rlimit {
                rlim_cur: 128,
                rlim_max: 128,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &lim) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().expect("spawn fd-starved norn serve");
    let socket_path = socket_path_for(&cache_home);
    wait_for_ready(&socket_path, Duration::from_secs(10));
    (child, socket_path, stderr_path, tmp)
}

/// Raw hello + initialize + find over a fresh connection, tolerant of a daemon
/// that is dying: returns `false` on any I/O failure instead of panicking.
fn try_open_vault(socket_path: &Path, vault: &Path) -> Option<UnixStream> {
    let stream = UnixStream::connect(socket_path).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .ok()?;
    let mut w = stream.try_clone().ok()?;
    let hello = serde_json::json!({
        "norn_control": "hello", "protocol": 1,
        "vault_root": vault.to_str()?,
    });
    let mut bytes = serde_json::to_vec(&hello).ok()?;
    bytes.push(b'\n');
    w.write_all(&bytes).ok()?;
    w.flush().ok()?;
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let ready: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if ready["norn_control"] != "ready" {
        return None;
    }
    // Best-effort MCP initialize + find to force a generation open (fd pressure).
    for req in [
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"vault.find","arguments":{"limit":10}}}),
    ] {
        let mut b = serde_json::to_vec(&req).ok()?;
        b.push(b'\n');
        w.write_all(&b).ok()?;
        w.flush().ok()?;
        let mut l = String::new();
        let _ = reader.read_line(&mut l);
    }
    // Keep the connection OPEN by returning the stream, so the context stays
    // resident and fds accumulate monotonically toward the limit.
    Some(stream)
}

/// A daemon that exhausts its fd table (a low `RLIMIT_NOFILE`) as distinct vaults
/// accumulate hits the poisoned (EMFILE) class and EXITS to self-heal, rather than
/// wedging and serving errors forever (NRN-337).
#[test]
fn fd_exhaustion_exits_to_self_heal() {
    let (mut child, socket_path, stderr_path, _tmp) = spawn_fd_starved_daemon();

    // Keep vault dirs AND connections alive so fds only grow.
    let mut vaults: Vec<TempDir> = Vec::new();
    let mut open_conns: Vec<UnixStream> = Vec::new();

    for i in 0..80 {
        // Bail out the moment the daemon has exited — the self-heal fired.
        if child.try_wait().expect("try_wait").is_some() {
            break;
        }
        let vault = seed_vault(i);
        if let Some(conn) = try_open_vault(&socket_path, vault.path()) {
            open_conns.push(conn);
        }
        vaults.push(vault);
        std::thread::sleep(Duration::from_millis(20));
    }

    // The daemon must have exited (exit IS the heal), not stayed wedged.
    let status = wait_for_exit(&mut child, Duration::from_secs(10));
    assert!(
        status.is_some(),
        "daemon must exit to self-heal on fd exhaustion, but it was still running"
    );

    // Exactly one clear self-heal line was logged.
    let log = std::fs::read_to_string(&stderr_path).unwrap_or_default();
    assert!(
        log.contains("poisoned state") && log.contains("exiting to self-heal"),
        "daemon stderr must carry the self-heal line, got:\n{log}"
    );
}
