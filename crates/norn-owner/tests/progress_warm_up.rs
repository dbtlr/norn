//! NRN-512, end to end: a request landing on a WARMING owner is answered with
//! `warming` progress frames on the one frame stream, then its terminal frame —
//! there is no separate pre-Ready path and no "vault not ready" early answer.
//!
//! This is the one-emitter rule observed from the wire: the same heartbeat that
//! paces a long apply paces warm-up, so the client's inter-frame silence budget
//! is satisfied by whichever phase happens to be running.
//!
//! Hermetic: a TempDir vault + a TempDir runtime dir and the internal
//! `NORN_OWNER_WARMUP_DELAY_MS` slow-build seam. This is the only test in this
//! binary, so the process-global env var it sets cannot race another.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use norn_wire::{ClientFrame, OwnerFrame, ProgressPhase};

fn connect(socket: &Utf8PathBuf) -> UnixStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match UnixStream::connect(socket.as_std_path()) {
            Ok(stream) => return stream,
            Err(e) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
                let _ = e;
            }
            Err(e) => panic!("owner never bound {socket}: {e}"),
        }
    }
}

#[test]
fn a_request_during_warm_up_rides_warming_progress_frames_then_its_answer() {
    let rt = tempfile::TempDir::new().unwrap();
    let runtime_dir = Utf8PathBuf::from_path_buf(rt.path().to_path_buf()).unwrap();

    // A NON-hidden `vault/` subdir: the graph walk skips hidden directories, so
    // warming `TempDir`'s own `.tmpXXXX` root would index zero documents.
    let vault_tmp = tempfile::TempDir::new().unwrap();
    let vault_root = Utf8PathBuf::from_path_buf(vault_tmp.path().join("vault")).unwrap();
    std::fs::create_dir(vault_root.as_std_path()).unwrap();
    std::fs::write(
        vault_root.join("a.md").as_std_path(),
        "---\ntype: note\n---\n",
    )
    .unwrap();

    // A warm-up longer than the heartbeat floor, so at least two beats land
    // before the build completes.
    std::env::set_var("NORN_OWNER_WARMUP_DELAY_MS", "2500");
    let socket_path = runtime_dir.join("h.fp.sock");
    let config = norn_owner::OwnerConfig {
        socket_path: socket_path.clone(),
        vault_root,
        idle_ttl: Duration::from_millis(400),
        build: None,
        config_path: None,
        events_dir: None,
    };
    let owner = std::thread::spawn(move || norn_owner::run(config).expect("owner run"));

    let stream = connect(&socket_path);
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = serde_json::to_vec(&ClientFrame::Probe).unwrap();
    line.push(b'\n');
    writer.write_all(&line).unwrap();
    writer.flush().unwrap();

    // Read the request's whole stream: progress frames until the terminal one.
    let mut phases = Vec::new();
    let terminal = loop {
        let mut raw = String::new();
        let read = reader.read_line(&mut raw).expect("owner frame");
        assert_ne!(read, 0, "owner closed before answering the probe");
        match serde_json::from_str::<OwnerFrame>(raw.trim()).expect("decodable frame") {
            OwnerFrame::Progress { progress } => phases.push(progress.phase),
            terminal => break terminal,
        }
    };

    assert!(
        phases.len() >= 2,
        "a ~2.5s warm-up must heartbeat more than once, got {phases:?}"
    );
    assert!(
        phases.iter().all(|p| *p == ProgressPhase::Warming),
        "a request queued behind warm-up reports `warming`, got {phases:?}"
    );
    assert!(
        matches!(terminal, OwnerFrame::Probe { .. }),
        "the request is answered once warm-up lands — never `vault not ready`, got {terminal:?}"
    );

    drop(reader);
    drop(writer);
    std::env::remove_var("NORN_OWNER_WARMUP_DELAY_MS");
    assert_eq!(owner.join().expect("owner thread"), 0);
}
