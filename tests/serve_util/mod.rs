//! Shared helpers for the `norn serve` end-to-end integration suite.
//!
//! Every test spawns its OWN daemon with its OWN `XDG_CACHE_HOME` tempdir, so
//! the daemon binds a private socket at `<cache>/norn/run/norn.sock`, fully
//! isolated from the developer's real daemon and from every other test. That
//! per-test isolation is what makes the suite safe to run in parallel without
//! `serial_test`: no two tests share a socket, a lock, or a cache.
//!
//! Readiness is proven by a deadline loop that polls socket existence AND a
//! successful control ping — never a fixed sleep. Every spawned child is killed
//! by a drop guard so a panicking test cannot leak a daemon.
//!
//! Everything here works in raw JSON (`serde_json::Value`) rather than the
//! crate's control-frame / MCP types, matching the repo's other integration
//! tests: the point is to exercise the real wire contract a foreign client sees.
//!
//! Cargo compiles this module into each `serve_*` test binary that declares
//! `mod serve_util;`. Not every binary uses every helper, so silence the
//! per-binary dead-code warnings here (clippy runs with `-D warnings`).
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

/// Path to the built `norn` binary — sibling of the test binary under
/// `target/<profile>/`. Mirrors the `norn_bin()` in the other integration tests.
pub fn norn_bin() -> PathBuf {
    let mut p = std::env::current_exe().expect("current_exe");
    p.pop(); // drop the test binary filename
    p.pop(); // drop the `deps/` component
    p.push(format!("norn{}", std::env::consts::EXE_SUFFIX));
    p
}

/// The well-known control socket the daemon binds under a given cache home:
/// `<cache_home>/norn/run/norn.sock`.
pub fn socket_path_for(cache_home: &Path) -> PathBuf {
    cache_home.join("norn").join("run").join("norn.sock")
}

/// Build a `norn serve` command wired to a private XDG cache + state dir. Stdio
/// defaults to null (the daemon's "listening" / "opened vault" lines are noise
/// for the tests); a caller that needs the refuse-message can override stderr.
pub fn build_serve_command(cache_home: &Path, state_home: &Path) -> Command {
    let mut cmd = Command::new(norn_bin());
    cmd.arg("serve")
        .env("XDG_CACHE_HOME", cache_home)
        .env("XDG_STATE_HOME", state_home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// A spawned child process that is killed and reaped on drop. Guards against a
/// leaked daemon when a test panics before an explicit shutdown.
pub struct ChildGuard(pub Child);

impl ChildGuard {
    pub fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A running `norn serve` daemon plus the tempdir backing its private socket.
/// Dropping it kills the daemon and removes the tempdir.
pub struct Daemon {
    pub child: ChildGuard,
    pub cache_home: PathBuf,
    pub state_home: PathBuf,
    pub socket_path: PathBuf,
    _tmp: TempDir,
}

impl Daemon {
    pub fn pid(&self) -> u32 {
        self.child.pid()
    }
}

/// Spawn a fresh daemon on its own private cache/state tempdir and wait until it
/// answers a ping. Panics if it is not ready within 10s.
pub fn spawn_ready_daemon() -> Daemon {
    let daemon = spawn_daemon();
    wait_for_ready(&daemon.socket_path, Duration::from_secs(10));
    daemon
}

/// Spawn a fresh daemon WITHOUT waiting for readiness (caller drives the wait).
pub fn spawn_daemon() -> Daemon {
    let tmp = tempfile::Builder::new()
        .prefix("norn-serve-e2e-")
        .tempdir()
        .expect("tempdir");
    let cache_home = tmp.path().join("cache");
    let state_home = tmp.path().join("state");
    let child = build_serve_command(&cache_home, &state_home)
        .spawn()
        .expect("spawn norn serve");
    let socket_path = socket_path_for(&cache_home);
    Daemon {
        child: ChildGuard(child),
        cache_home,
        state_home,
        socket_path,
        _tmp: tmp,
    }
}

/// Poll until the socket exists AND answers a well-formed pong, or panic at the
/// deadline. Deadline loop with 10ms steps — no fixed sleeps.
pub fn wait_for_ready(socket_path: &Path, within: Duration) {
    let deadline = Instant::now() + within;
    loop {
        if socket_path.exists() {
            if let Some(pong) = try_ping(socket_path) {
                if pong["norn_control"] == "pong" && pong["protocol"] == 1 {
                    return;
                }
            }
        }
        if Instant::now() >= deadline {
            panic!(
                "daemon socket not ready within {within:?} at {}",
                socket_path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Send one control ping and return the parsed pong frame, or `None` on any
/// failure (not yet bound, connection refused, timeout, malformed reply).
pub fn try_ping(socket_path: &Path) -> Option<Value> {
    try_ping_timed(socket_path).0
}

/// Like [`try_ping`] but also reports how long the ping round-trip took (used by
/// the load test to bound ping latency under query load).
pub fn try_ping_timed(socket_path: &Path) -> (Option<Value>, Duration) {
    let started = Instant::now();
    let pong = (|| {
        let mut stream = UnixStream::connect(socket_path).ok()?;
        // Generous internal timeout so the CALLER can measure real latency and
        // assert its own (tighter) bound rather than being clamped here.
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .ok()?;
        stream
            .write_all(b"{\"norn_control\":\"ping\",\"protocol\":1}\n")
            .ok()?;
        stream.flush().ok()?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        serde_json::from_str::<Value>(line.trim()).ok()
    })();
    (pong, started.elapsed())
}

/// Poll `child.try_wait()` until the process exits or the deadline elapses.
/// Returns the exit status, or `None` if it was still running at the deadline.
pub fn wait_for_exit(child: &mut Child, within: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + within;
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => return Some(status),
            None => {
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// Send `signal` to `pid` via `libc::kill`.
pub fn send_signal(pid: u32, signal: libc::c_int) {
    // SAFETY: `kill(2)` with a pid we own and a valid signal number.
    let rc = unsafe { libc::kill(pid as libc::pid_t, signal) };
    assert_eq!(
        rc,
        0,
        "kill({pid}, {signal}) failed: {}",
        std::io::Error::last_os_error()
    );
}

/// A live MCP connection to the daemon for one vault: the `hello`/`ready`
/// handshake is already done and JSON-RPC flows over the same stream.
pub struct Conn {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: i64,
}

/// Open a connection, send `hello` for `vault_root`, and require a `ready`
/// frame. The returned [`Conn`] is positioned to speak MCP JSON-RPC.
pub fn connect_and_hello(socket_path: &Path, vault_root: &Path) -> Conn {
    let stream = UnixStream::connect(socket_path).expect("connect to daemon socket");
    // Bounded I/O so a wedged daemon fails the test instead of hanging it.
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut conn = Conn {
        stream,
        reader,
        next_id: 1,
    };

    let hello = serde_json::json!({
        "norn_control": "hello",
        "protocol": 1,
        "vault_root": vault_root.to_str().expect("utf8 vault root"),
    });
    conn.write_line(&hello);

    let ready = conn
        .read_json_line()
        .expect("expected a ready control frame after hello");
    assert_eq!(
        ready["norn_control"], "ready",
        "expected ready frame, got {ready}"
    );
    assert_eq!(ready["protocol"], 1, "ready protocol must be 1: {ready}");
    assert!(
        ready["version"].is_string(),
        "ready frame must carry a version string: {ready}"
    );
    conn
}

impl Conn {
    fn write_line(&mut self, v: &Value) {
        let mut bytes = serde_json::to_vec(v).expect("serialize frame");
        bytes.push(b'\n');
        self.stream.write_all(&bytes).expect("write frame");
        self.stream.flush().expect("flush frame");
    }

    /// Read one newline-delimited JSON value, skipping blank / non-JSON lines.
    fn read_json_line(&mut self) -> Option<Value> {
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).ok()?;
            if n == 0 {
                return None;
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
                return Some(v);
            }
            // Non-JSON noise on the wire: skip and keep reading.
        }
    }

    /// Send a JSON-RPC request and return the response matching its id
    /// (skipping any notifications or out-of-order responses).
    pub fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_line(&req);
        for _ in 0..1000 {
            let v = self
                .read_json_line()
                .unwrap_or_else(|| panic!("connection closed awaiting response to {method}"));
            if v["id"] == id {
                return v;
            }
        }
        panic!("no JSON-RPC response for id {id} ({method})");
    }

    /// Run the MCP `initialize` handshake. Returns the response.
    pub fn initialize(&mut self) -> Value {
        self.rpc(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "serve-e2e", "version": "0.0.1" },
            }),
        )
    }

    /// Invoke an MCP tool. Returns the raw JSON-RPC response.
    pub fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        self.rpc(
            "tools/call",
            serde_json::json!({ "name": name, "arguments": arguments }),
        )
    }
}

/// Extract the `documents` array from a successful `vault.find` response,
/// panicking (with the whole response) if the call errored or the shape is off.
pub fn find_documents(resp: &Value) -> Vec<Value> {
    assert!(
        resp.get("error").is_none(),
        "vault.find must not error, got: {resp}"
    );
    resp["result"]["structuredContent"]["documents"]
        .as_array()
        .unwrap_or_else(|| panic!("vault.find result must carry a documents array, got: {resp}"))
        .clone()
}

/// Extract the `records` array from a successful `vault.get` response.
pub fn get_records(resp: &Value) -> Vec<Value> {
    assert!(
        resp.get("error").is_none(),
        "vault.get must not error, got: {resp}"
    );
    resp["result"]["structuredContent"]["records"]
        .as_array()
        .unwrap_or_else(|| panic!("vault.get result must carry a records array, got: {resp}"))
        .clone()
}

/// Sorted `path` strings of a documents/records array.
pub fn paths(docs: &[Value]) -> Vec<String> {
    let mut ps: Vec<String> = docs
        .iter()
        .map(|d| d["path"].as_str().unwrap_or("").to_string())
        .collect();
    ps.sort();
    ps
}

/// Read a whole file to a string (tests are allowed to read their temp vault).
pub fn read_to_string(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}
