//! NRN-512: cancellation is out of the model. A client that disconnects
//! mid-mutation does NOT abort the work — the owner completes the plan under
//! per-file atomicity, and the caller resolves the resulting uncertainty by
//! reading the vault (ADR 0011), never by inheriting a half-applied one.
//!
//! Hermetic: a TempDir vault + a TempDir runtime dir, the owner run in-process
//! on its own thread. The client writes a CONFIRMED `set` frame, drops the
//! socket without reading a byte of the reply, and the vault is then polled for
//! the write.
//!
//! The assertion does not depend on winning a race with the owner: the dispatch
//! is AWAITED before the connection is touched again, so nothing consults the
//! peer between reading the frame and finishing the work. Whether the drop lands
//! before or after the owner tried to reply, the write must be there.

#![cfg(unix)]

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use norn_wire::{ClientFrame, SetParams};

/// Connect with bounded retry while the owner binds its socket.
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
fn a_client_disconnecting_mid_mutation_still_gets_the_write_applied() {
    let rt = tempfile::TempDir::new().unwrap();
    let runtime_dir = Utf8PathBuf::from_path_buf(rt.path().to_path_buf()).unwrap();

    // A NON-hidden `vault/` subdir: `TempDir` names its dir `.tmpXXXX` and the
    // graph walk skips hidden directories, so a dot-prefixed root would index
    // zero documents and the mutation would refuse `target-not-found`.
    let vault_tmp = tempfile::TempDir::new().unwrap();
    let vault_root = Utf8PathBuf::from_path_buf(vault_tmp.path().join("vault")).unwrap();
    std::fs::create_dir(vault_root.as_std_path()).unwrap();
    let doc = vault_root.join("a.md");
    std::fs::write(doc.as_std_path(), "---\nstatus: draft\n---\nbody\n").unwrap();

    let socket_path = runtime_dir.join("h.fp.sock");
    let config = norn_owner::OwnerConfig {
        socket_path: socket_path.clone(),
        vault_root: vault_root.clone(),
        // Short enough that the owner reaps itself once the (single) client is
        // gone, so the thread below joins without a shutdown signal.
        idle_ttl: Duration::from_millis(400),
        build: None,
        config_path: None,
        events_dir: None,
    };
    let owner = std::thread::spawn(move || norn_owner::run(config).expect("owner run"));

    // Send a CONFIRMED mutation, then vanish. No read, no shutdown handshake —
    // the socket is simply dropped, which is what a killed client looks like.
    {
        let mut stream = connect(&socket_path);
        let frame = ClientFrame::Set {
            params: SetParams {
                target: "a.md".into(),
                fields: vec!["status=done".into()],
                confirm: true,
                ..Default::default()
            },
        };
        let mut line = serde_json::to_vec(&frame).unwrap();
        line.push(b'\n');
        stream.write_all(&line).unwrap();
        stream.flush().unwrap();
    }

    // The owner completes the plan regardless. Poll rather than sleep-and-check
    // so the test is not tied to warm-up + apply timing.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last = String::new();
    let applied = loop {
        last = std::fs::read_to_string(doc.as_std_path()).unwrap_or(last);
        if last.contains("status: done") {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        applied,
        "a disconnect mid-mutation must not abort the work; document reads:\n{last}"
    );

    assert_eq!(
        owner.join().expect("owner thread"),
        0,
        "an unread reply is not an owner fault — the idle reap is a clean exit"
    );
}
