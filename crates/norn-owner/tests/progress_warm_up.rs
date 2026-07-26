//! NRN-512, end to end: a request landing on a WARMING owner is answered with
//! `warming` progress frames on the one frame stream, then its terminal frame —
//! there is no separate pre-Ready path and no "vault not ready" early answer.
//!
//! This is the one-emitter rule observed from the wire: the same heartbeat that
//! paces a long apply paces warm-up, so the client's silence budget is satisfied
//! by whichever phase happens to be running. It also pins the other half of the
//! protocol — that the terminal frame is LAST — which is what the owner buys by
//! stopping and joining the heartbeat before it writes that frame.
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
    // A generous deadline: it exists so a protocol regression FAILS instead of
    // hanging this test forever, not as a timing assertion.
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = serde_json::to_vec(&ClientFrame::Probe).unwrap();
    line.push(b'\n');
    writer.write_all(&line).unwrap();
    writer.flush().unwrap();

    // Read the request's whole stream: progress frames until the terminal one.
    let mut observed = Vec::new();
    let terminal = loop {
        let mut raw = String::new();
        let read = reader.read_line(&mut raw).expect("owner frame");
        assert_ne!(read, 0, "owner closed before answering the probe");
        match serde_json::from_str::<OwnerFrame>(raw.trim()).expect("decodable frame") {
            OwnerFrame::Progress { progress } => observed.push(progress),
            terminal => break terminal,
        }
    };

    assert!(
        observed.len() >= 2,
        "a ~2.5s warm-up must heartbeat more than once, got {observed:?}"
    );
    assert!(
        observed.iter().all(|p| p.phase == ProgressPhase::Warming),
        "a request queued behind warm-up reports `warming`, got {observed:?}"
    );
    assert!(
        observed
            .iter()
            .all(|p| p.done.is_none() && p.total.is_none()),
        "warming carries no units — the frame itself is the fact, got {observed:?}"
    );
    assert!(
        matches!(terminal, OwnerFrame::Probe { .. }),
        "the request is answered once warm-up lands — never `vault not ready`, got {terminal:?}"
    );

    // NOTHING follows the terminal frame. Closing the write half ends the
    // owner's connection loop, so the next read is EOF — unless a beat slipped
    // out after the answer, in which case it is sitting in this buffer and is
    // read here instead. That is exactly what stopping and JOINING the heartbeat
    // before writing the terminal frame buys: a client can treat the terminal
    // frame as the end of the request without tolerating a trailing beat.
    writer.shutdown(std::net::Shutdown::Write).unwrap();
    let mut trailing = String::new();
    let read = reader.read_line(&mut trailing).expect("stream end");
    assert_eq!(read, 0, "a frame followed the terminal frame: {trailing:?}");

    drop(reader);
    drop(writer);
    std::env::remove_var("NORN_OWNER_WARMUP_DELAY_MS");
    assert_eq!(owner.join().expect("owner thread"), 0);
}
