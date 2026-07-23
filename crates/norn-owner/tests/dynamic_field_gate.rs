//! NRN-367: the owner gates a query's dynamically-desugared field keys against
//! the vault's field universe (schema-declared fields ∪ observed frontmatter
//! keys). A genuinely-unknown dynamic field is answered with an
//! `OwnerFrame::Rejected` whose message names the field and whose `hints` carry
//! the did-you-mean (via the one shared `closest` heuristic) — the owner stays
//! alive. A VALID field that merely matches nothing is NOT gated: it runs to an
//! empty `Find` report at the normal success path.
//!
//! This pins the owner-side half of the gate end-to-end (warm cache → gate →
//! frame). The CLI-visible surface (`norn: unknown field …` + `hint:`, exit 1,
//! empty stdout) is pinned against the built bin in `norn`'s `cli` suite, and
//! the pure gate/did-you-mean logic in `norn-core`'s grammar unit tests.
//! Hermetic: TempDir vault + TempDir runtime dir, a raw Unix-socket wire
//! exchange (no dev-dep on norn-wire/serde_json).

#[cfg(unix)]
#[test]
fn unknown_dynamic_field_is_rejected_with_a_did_you_mean_hint() {
    use camino::Utf8PathBuf;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    let rt = tempfile::TempDir::new().unwrap();
    let runtime_dir = Utf8PathBuf::from_path_buf(rt.path().to_path_buf()).unwrap();

    // A valid vault whose documents carry `title` / `status` in frontmatter —
    // both become observed field-universe members. The root is a NON-hidden
    // `vault/` subdir: `TempDir` names its dir `.tmpXXXX` (dot-prefixed) and the
    // graph walk skips hidden directories, so warming a dot-prefixed root
    // directly would index zero documents.
    let vault_tmp = tempfile::TempDir::new().unwrap();
    let vault_root =
        Utf8PathBuf::from_path_buf(vault_tmp.path().join("vault").to_path_buf()).unwrap();
    std::fs::create_dir(vault_root.as_std_path()).unwrap();
    std::fs::write(
        vault_root.join("a.md").as_std_path(),
        "---\ntype: note\ntitle: Hello\nstatus: active\n---\nbody\n",
    )
    .unwrap();

    let socket_path = runtime_dir.join("h.fp.sock");
    let socket_std = socket_path.as_std_path().to_path_buf();
    // A short idle TTL so the owner reaps promptly after the last request — each
    // request resets the idle clock, so a couple-second TTL comfortably outlives
    // the back-to-back exchange below while keeping the test quick.
    let config = norn_owner::OwnerConfig {
        socket_path,
        vault_root,
        idle_ttl: Duration::from_secs(2),
        build: None,
        config_path: None,
        events_dir: None,
    };
    let handle = std::thread::spawn(move || norn_owner::run(config).expect("owner run"));

    // Wait for the socket to bind.
    let start = Instant::now();
    while !socket_std.exists() && start.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(socket_std.exists(), "owner never bound its socket");

    // One request/response over a fresh connection.
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

    // Ping until the warm-up reaches `ready` (a valid vault warms promptly).
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let pong = round_trip(r#"{"op":"ping","protocol":1}"#);
        if pong.contains(r#""serving":"ready""#) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // An unknown dynamic field (`titel`, a typo of `title`) → Rejected with a
    // headline naming the field and a did-you-mean hint pointing at `title`.
    let rejected = round_trip(r#"{"op":"find","params":{"dynamic_keys":["titel"]}}"#);
    assert!(
        rejected.contains(r#""op":"rejected""#),
        "expected a rejected frame, got {rejected:?}"
    );
    assert!(
        rejected.contains("unknown field `titel`"),
        "expected the headline to name the unknown field, got {rejected:?}"
    );
    assert!(
        rejected.contains("did you mean `title`?"),
        "expected a did-you-mean hint for `title`, got {rejected:?}"
    );

    // A VALID field (`status`) that matches nothing is NOT gated — the normal
    // success path returns a Find report with an empty document set, exit 0.
    let empty = round_trip(
        r#"{"op":"find","params":{"filter":{"eq":["status:backlog"]},"dynamic_keys":["status"]}}"#,
    );
    assert!(
        empty.contains(r#""op":"find""#),
        "a known field must run the query, not reject, got {empty:?}"
    );
    assert!(
        empty.contains(r#""total":0"#),
        "the valid field matches nothing → empty report, got {empty:?}"
    );

    // The owner idle-reaps a couple seconds after the last request — a clean
    // exit (never the fatal exit-to-heal code): a rejected query is a user error,
    // not an owner-health event.
    let code = handle.join().unwrap();
    assert_eq!(
        code, 0,
        "a dynamic-field rejection must not crash the owner"
    );
    assert!(
        !socket_std.exists(),
        "the reaped owner left a stale socket behind"
    );
}

/// NRN-374: `describe` shares `classify_read`'s uniform `FieldRejection`
/// signature (same as `find`/`count`) but was not wired to the field-universe
/// gate at all — an unknown dynamic field silently reached the data-mode
/// summary instead of rejecting. This pins the owner-side wiring end-to-end,
/// the `describe` counterpart to
/// `unknown_dynamic_field_is_rejected_with_a_did_you_mean_hint` above.
#[cfg(unix)]
#[test]
fn describe_unknown_dynamic_field_is_rejected_with_a_did_you_mean_hint() {
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
        "---\ntype: note\ntitle: Hello\nstatus: active\n---\nbody\n",
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

    // An unknown dynamic field on `describe` → Rejected with a headline naming
    // the field and a did-you-mean hint, exactly like `find`/`count`.
    let rejected = round_trip(r#"{"op":"describe","params":{"dynamic_keys":["titel"]}}"#);
    assert!(
        rejected.contains(r#""op":"rejected""#),
        "expected a rejected frame, got {rejected:?}"
    );
    assert!(
        rejected.contains("unknown field `titel`"),
        "expected the headline to name the unknown field, got {rejected:?}"
    );
    assert!(
        rejected.contains("did you mean `title`?"),
        "expected a did-you-mean hint for `title`, got {rejected:?}"
    );

    // A VALID field is NOT gated — `describe` runs to its normal structure
    // report (folders/schema always present) rather than rejecting.
    let ok = round_trip(r#"{"op":"describe","params":{"dynamic_keys":["status"]}}"#);
    assert!(
        ok.contains(r#""op":"describe""#),
        "a known field must run describe, not reject, got {ok:?}"
    );

    let code = handle.join().unwrap();
    assert_eq!(
        code, 0,
        "a dynamic-field rejection must not crash the owner"
    );
    assert!(
        !socket_std.exists(),
        "the reaped owner left a stale socket behind"
    );
}
