//! NRN-360: a present-but-invalid `.norn/config.yaml` is a USER error, not a
//! crashed owner. The warm-up must NOT go_fatal (exit-to-heal); instead the
//! owner answers every frame with the config error as a `Rejected` and then
//! EAGER-REAPS — once a client has the rejection in hand it latches a clean
//! shutdown (exit 0), so a resummon re-reads a fixed config immediately rather
//! than reconnecting to a stale-error owner for the whole idle TTL.
//!
//! The oracle (v0.48.1) surfaces the same class as, on stderr, exit 1:
//!
//!   invalid config <abs>/.norn/config.yaml: unknown field `bogus`, expected …
//!
//! This test pins OUR owner-side lifecycle (the rejection message shape + a
//! clean, PROMPT reap under a deliberately long idle TTL). The client-visible
//! surface (`ClientError::Rejected` + fix-pickup) is pinned in `norn-client`'s
//! summon-lifecycle suite, and the end-to-end CLI surface (`norn: invalid
//! config …`, exit 1) is pinned against the built bin in `norn`'s `cli` suite.
//! Hermetic: TempDir vault + TempDir runtime dir, a raw Unix-socket wire
//! exchange (no dev-dep on norn-wire/serde_json).

#[cfg(unix)]
#[test]
fn bad_config_warm_up_rejects_the_client_then_eager_reaps() {
    use camino::Utf8PathBuf;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    let rt = tempfile::TempDir::new().unwrap();
    let runtime_dir = Utf8PathBuf::from_path_buf(rt.path().to_path_buf()).unwrap();

    let vault_tmp = tempfile::TempDir::new().unwrap();
    let vault_root = Utf8PathBuf::from_path_buf(vault_tmp.path().to_path_buf()).unwrap();
    // A present-but-invalid config: a well-formed-YAML file with an unknown
    // top-level field — the schema-invalid class the oracle rejects.
    std::fs::create_dir_all(vault_root.join(".norn").as_std_path()).unwrap();
    std::fs::write(
        vault_root.join(".norn/config.yaml").as_std_path(),
        "bogus: true\n",
    )
    .unwrap();
    std::fs::write(
        vault_root.join("a.md").as_std_path(),
        "---\ntype: note\n---\n",
    )
    .unwrap();

    let socket_path = runtime_dir.join("h.fp.sock");
    let socket_std = socket_path.as_std_path().to_path_buf();
    // A deliberately LONG idle TTL: if the owner merely idle-reaped it would
    // live 30s. Eager-reap must beat that by orders of magnitude, so the timed
    // `join` below is the discriminator between the two.
    const TTL: Duration = Duration::from_secs(30);
    let config = norn_owner::OwnerConfig {
        socket_path,
        vault_root,
        idle_ttl: TTL,
        build: None,
        config_path: None,
    };

    let run_started = Instant::now();
    let handle = std::thread::spawn(move || norn_owner::run(config).expect("owner run"));

    // Wait for the socket to bind (the owner binds before warm-up runs).
    let start = Instant::now();
    while !socket_std.exists() && start.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(socket_std.exists(), "owner never bound its socket");

    // Ping until the bad-config warm-up lands: it may briefly answer a
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

    let line = rejected_message.expect("the bad-config owner must reject the ping, not go away");
    // The rejection carries the oracle-shaped config message verbatim.
    assert!(
        line.contains("invalid config "),
        "expected the oracle config-error message, got {line:?}"
    );
    assert!(
        line.contains("unknown field `bogus`"),
        "expected the serde detail, got {line:?}"
    );

    // EAGER REAP: `run` returns as soon as the client is served, exit 0 (never
    // the fatal exit-to-heal code 1), WELL under the 30s idle TTL — proof the
    // owner did not linger on its idle clock.
    let code = handle.join().unwrap();
    let reap_latency = run_started.elapsed();
    assert_eq!(
        code, 0,
        "a bad-config warm-up must reap cleanly, not go fatal (exit 1)"
    );
    assert!(
        reap_latency < Duration::from_secs(15),
        "the bad-config owner must eager-reap, not wait out the {TTL:?} idle TTL (took {reap_latency:?})"
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
