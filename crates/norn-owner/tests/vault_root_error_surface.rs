//! NRN-414: a missing (or non-directory) vault root is a USER error, not a
//! crashed owner. When the summoner spawns an owner against a root that does not
//! exist — a bad `-C`/NORN_ROOT, or a registered root that vanished after the
//! client resolved it — the warm-up graph build would fail. That failure is
//! classified as a user error, NOT exit-to-heal: the owner answers every frame
//! with the graph-build message (`vault root does not exist: <path>`) as a
//! `Rejected` and then EAGER-REAPS (exit 0), exactly like the bad-config family.
//!
//! This test pins OUR owner-side lifecycle: the rejection message shape carrying
//! the graph builder's own wording, plus a clean, PROMPT reap under a
//! deliberately long idle TTL, with no zombie owner (stale socket) and no
//! orphaned db dir left behind. The client-side instant precheck (which prevents
//! most summons from ever reaching this fail-safe) is pinned in `norn-cli`'s
//! `routed` unit tests. Hermetic: a TempDir runtime dir and a nonexistent vault
//! root under it, a raw Unix-socket wire exchange (no dev-dep on
//! norn-wire/serde_json).

#[cfg(unix)]
#[test]
fn missing_vault_root_warm_up_rejects_the_client_then_eager_reaps() {
    use camino::Utf8PathBuf;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    let rt = tempfile::TempDir::new().unwrap();
    let runtime_dir = Utf8PathBuf::from_path_buf(rt.path().to_path_buf()).unwrap();

    // A vault root that does not exist: a never-created child of the runtime dir.
    // The warm-up's graph-build pre-check classifies it as a user error.
    let vault_root = runtime_dir.join("nrn414-does-not-exist");
    assert!(!vault_root.as_std_path().exists());

    let socket_path = runtime_dir.join("h.fp.sock");
    let socket_std = socket_path.as_std_path().to_path_buf();
    // A deliberately LONG idle TTL: if the owner merely idle-reaped it would live
    // 30s. Eager-reap must beat that by orders of magnitude, so the timed `join`
    // below is the discriminator between the two.
    const TTL: Duration = Duration::from_secs(30);
    let config = norn_owner::OwnerConfig {
        socket_path,
        vault_root,
        idle_ttl: TTL,
        build: None,
        config_path: None,
        events_dir: None,
    };

    let run_started = Instant::now();
    let handle = std::thread::spawn(move || norn_owner::run(config).expect("owner run"));

    // Wait for the socket to bind (the owner binds before warm-up runs).
    let start = Instant::now();
    while !socket_std.exists() && start.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(socket_std.exists(), "owner never bound its socket");

    // Ping until the missing-root warm-up lands: it may briefly answer a
    // Pong(cold/opening) before the failure is recorded, then rejects every
    // frame. A fresh connection per attempt.
    let mut rejected_message: Option<String> = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && rejected_message.is_none() {
        let Ok(stream) = UnixStream::connect(&socket_std) else {
            break;
        };
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        writer.write_all(br#"{"op":"ping","protocol":1}"#).unwrap();
        writer.write_all(b"\n").unwrap();
        writer.flush().unwrap();
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if line.contains(r#""op":"rejected""#) {
            rejected_message = Some(line);
        } else {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    let line = rejected_message.expect("the missing-root owner must reject the ping, not go away");
    // The rejection carries the graph builder's own message verbatim.
    assert!(
        line.contains("vault root does not exist"),
        "expected the graph-build missing-root message, got {line:?}"
    );

    // EAGER REAP: `run` returns as soon as the client is served, exit 0 (never
    // the fatal exit-to-heal code 1), WELL under the 30s idle TTL — proof the
    // owner did not linger on its idle clock.
    let code = handle.join().unwrap();
    let reap_latency = run_started.elapsed();
    assert_eq!(
        code, 0,
        "a missing-root warm-up must reap cleanly, not go fatal (exit 1)"
    );
    assert!(
        reap_latency < Duration::from_secs(15),
        "the missing-root owner must eager-reap, not wait out the {TTL:?} idle TTL (took {reap_latency:?})"
    );
    assert!(
        !socket_std.exists(),
        "the reaped owner left a stale socket behind"
    );
    let orphans = std::fs::read_dir(rt.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("norn-owner-db-")
        })
        .count();
    assert_eq!(orphans, 0, "the failed warm-up orphaned a db dir");
}
