//! NRN-460: `get { format: markdown }` refuses a multi-document selection at the
//! owner/wire seam, not only in the CLI renderer. Both the CLI (`norn get
//! --format markdown`) and the MCP `vault.get` tool route their `Get` frame
//! through this same owner, so a raw wire exchange pins the shared contract:
//! two resolved documents answers with an `error`-severity note and no
//! `markdown_content`, never a silent structured dump.
//!
//! Hermetic: TempDir vault + TempDir runtime dir, a raw Unix-socket wire
//! exchange (no dev-dep on norn-wire/serde_json) — same shape as
//! `dynamic_field_gate.rs`.

#[cfg(unix)]
#[test]
fn markdown_format_refuses_a_multi_document_selection() {
    use camino::Utf8PathBuf;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    let rt = tempfile::TempDir::new().unwrap();
    let runtime_dir = Utf8PathBuf::from_path_buf(rt.path().to_path_buf()).unwrap();

    let vault_tmp = tempfile::TempDir::new().unwrap();
    let vault_root =
        Utf8PathBuf::from_path_buf(vault_tmp.path().join("vault").to_path_buf()).unwrap();
    std::fs::create_dir(vault_root.as_std_path()).unwrap();
    std::fs::write(
        vault_root.join("a.md").as_std_path(),
        "---\ntype: note\ntitle: A\n---\nbody a\n",
    )
    .unwrap();
    std::fs::write(
        vault_root.join("b.md").as_std_path(),
        "---\ntype: note\ntitle: B\n---\nbody b\n",
    )
    .unwrap();

    let socket_path = runtime_dir.join("h.fp.sock");
    let socket_std = socket_path.as_std_path().to_path_buf();
    let config = norn_owner::OwnerConfig {
        socket_path,
        vault_root,
        idle_ttl: Duration::from_secs(2),
        build: None,
        config_path: None,
        events_dir: None,
    };
    let handle = std::thread::spawn(move || norn_owner::run(config).expect("owner run"));

    let start = Instant::now();
    while !socket_std.exists() && start.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(socket_std.exists(), "owner never bound its socket");

    let round_trip = |request: &str| -> String {
        let stream = UnixStream::connect(&socket_std).expect("connect");
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        writer.write_all(request.as_bytes()).unwrap();
        writer.write_all(b"\n").unwrap();
        writer.flush().unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        line
    };

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let pong = round_trip(r#"{"op":"ping","protocol":1}"#);
        if pong.contains(r#""serving":"ready""#) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // Two targets, each resolving to its own document, `format: markdown`
    // (`markdown: true` on the wire): the resolution itself is clean — no
    // ambiguous-stem or missing-target notes — so the ONLY note on the report is
    // the multi-selection refusal this seam now owns.
    let response =
        round_trip(r#"{"op":"get","params":{"targets":["a.md","b.md"],"markdown":true}}"#);
    assert!(
        response.contains(r#""op":"get""#),
        "expected a Get report frame (the refusal rides in the report's notes, \
         not a Rejected frame), got {response:?}"
    );
    assert!(
        response.contains(r#""severity":"error""#)
            && response.contains("format-markdown-multi-selection"),
        "expected an error-severity 'format-markdown-multi-selection' note, got {response:?}"
    );
    assert!(
        response.contains("--format markdown returns a single document; 2 selected"),
        "expected the refusal message to name the selected count, got {response:?}"
    );
    assert!(
        !response.contains("markdown_content"),
        "a refused multi-selection must carry no markdown_content, got {response:?}"
    );
    // Both resolved documents' records still ride the report (mirrors the
    // established missing-target note contract: notes annotate, they never
    // truncate the record set).
    assert!(
        response.contains("a.md") && response.contains("b.md"),
        "both resolved records must still be present, got {response:?}"
    );

    let code = handle.join().unwrap();
    assert_eq!(code, 0, "a clean refusal must not crash the owner");
    assert!(
        !socket_std.exists(),
        "the reaped owner left a stale socket behind"
    );
}

/// The single-document case is unaffected: `format: markdown` for exactly one
/// resolved target still reads the source bytes onto `markdown_content`.
#[cfg(unix)]
#[test]
fn markdown_format_reads_a_single_document() {
    use camino::Utf8PathBuf;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    let rt = tempfile::TempDir::new().unwrap();
    let runtime_dir = Utf8PathBuf::from_path_buf(rt.path().to_path_buf()).unwrap();

    let vault_tmp = tempfile::TempDir::new().unwrap();
    let vault_root =
        Utf8PathBuf::from_path_buf(vault_tmp.path().join("vault").to_path_buf()).unwrap();
    std::fs::create_dir(vault_root.as_std_path()).unwrap();
    std::fs::write(
        vault_root.join("a.md").as_std_path(),
        "---\ntype: note\ntitle: A\n---\nbody a\n",
    )
    .unwrap();

    let socket_path = runtime_dir.join("h.fp.sock");
    let socket_std = socket_path.as_std_path().to_path_buf();
    let config = norn_owner::OwnerConfig {
        socket_path,
        vault_root,
        idle_ttl: Duration::from_secs(2),
        build: None,
        config_path: None,
        events_dir: None,
    };
    let handle = std::thread::spawn(move || norn_owner::run(config).expect("owner run"));

    let start = Instant::now();
    while !socket_std.exists() && start.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(socket_std.exists(), "owner never bound its socket");

    let round_trip = |request: &str| -> String {
        let stream = UnixStream::connect(&socket_std).expect("connect");
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        writer.write_all(request.as_bytes()).unwrap();
        writer.write_all(b"\n").unwrap();
        writer.flush().unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        line
    };

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let pong = round_trip(r#"{"op":"ping","protocol":1}"#);
        if pong.contains(r#""serving":"ready""#) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let response = round_trip(r#"{"op":"get","params":{"targets":["a.md"],"markdown":true}}"#);
    assert!(
        response.contains(r#""markdown_content":"---\ntype: note\ntitle: A\n---\nbody a\n""#),
        "expected the exact source bytes on markdown_content, got {response:?}"
    );
    assert!(
        !response.contains("format-markdown-multi-selection"),
        "a single resolved document must not carry the multi-selection note, got {response:?}"
    );

    let code = handle.join().unwrap();
    assert_eq!(code, 0, "a clean read must not crash the owner");
    assert!(
        !socket_std.exists(),
        "the reaped owner left a stale socket behind"
    );
}
