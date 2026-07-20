//! End-to-end summon lifecycle (NRN-345), driving the REAL owner (the `norn`
//! bin in owner mode) over a real Unix socket — no read verbs, just the routed
//! probe + control plane:
//!
//!   summon -> owner builds -> serve a trivial routed request -> idle-reap ->
//!   db + socket gone.
//!
//! Hermetic: TempDir vault, a TempDir runtime dir (never the user's real
//! `XDG_RUNTIME_DIR`), a short idle TTL via config, and bounded waits on
//! conditions (never sleeps-as-synchronization).

mod common;

use std::path::PathBuf;
use std::time::Duration;

use norn_client::{open, socket_path, ClientError, SummonConfig};
use norn_wire::ServingState;

fn base_config(vault_root: PathBuf, runtime_dir: PathBuf, ttl: Duration) -> SummonConfig {
    SummonConfig {
        vault_root,
        runtime_dir,
        // A fixed test fingerprint keeps the socket name stable regardless of
        // the test-binary path; isolation across concurrent tests comes from the
        // per-test TempDir runtime dir, not the fingerprint.
        fingerprint: "testfingerprnt0".to_string(),
        idle_ttl: ttl,
        owner_exe: common::norn_bin(),
        connect_budget: Duration::from_secs(15),
        config_override: None,
    }
}

#[test]
fn summon_builds_serves_then_idle_reaps_deleting_db_and_socket() {
    let (_vault_tmp, vault_root) = common::temp_vault(3);
    let rt_tmp = tempfile::TempDir::new().unwrap();
    let runtime_dir = rt_tmp.path().to_path_buf();

    // Short idle TTL so the reap is observable within the test budget.
    let config = base_config(vault_root, runtime_dir.clone(), Duration::from_secs(1));

    // Summon: no owner is live, so `open` spawns one and connects as it binds.
    let mut session = open(&config).expect("summon-or-connect should succeed");
    let socket = session.socket().to_path_buf();
    assert!(socket.exists(), "owner should have bound its socket");

    // Warm-up on summon: cold/opening -> ready.
    let pong = session
        .wait_until_ready(Duration::from_secs(20))
        .expect("owner should warm up to ready");
    assert_eq!(pong.serving, ServingState::Ready);

    // The born-with-owner db exists while the owner lives.
    assert_eq!(
        common::owner_db_dirs(&runtime_dir),
        1,
        "the owner's db dir should exist while it serves"
    );

    // Serve a trivial routed request through the owner's warm serve_read.
    let count = session.probe().expect("probe should serve");
    assert_eq!(count, 3, "probe should count the 3 seeded notes");

    // Drop the session so nothing keeps the owner busy; the idle TTL then fires.
    drop(session);

    // Idle-reap: the owner unbinds the socket and deletes its db. Bounded wait.
    let reaped = common::wait_until(Duration::from_secs(15), || {
        !socket.exists() && common::owner_db_dirs(&runtime_dir) == 0
    });
    assert!(
        reaped,
        "owner should idle-reap: socket exists={}, db dirs={}",
        socket.exists(),
        common::owner_db_dirs(&runtime_dir),
    );
}

/// NRN-384 (F1): a HELD session must survive the owner's idle-reap. A long-lived
/// holder (the MCP stdio server) keeps one session for its whole lifetime; once
/// the owner idle-reaps between calls, the next request fails and the session
/// would be permanently bricked without recovery. `resummon` re-establishes a
/// fresh, ready owner so the very next call succeeds again — proven end to end
/// against the real owner: summon → serve → idle-reap (while still holding the
/// session) → resummon → serve again.
#[test]
fn held_session_resummons_after_idle_reap() {
    let (_vault_tmp, vault_root) = common::temp_vault(3);
    let rt_tmp = tempfile::TempDir::new().unwrap();
    let runtime_dir = rt_tmp.path().to_path_buf();

    // Short idle TTL so the reap is observable within the test budget; the held
    // session stays idle past it (no request in flight → the reaper fires).
    let config = base_config(vault_root, runtime_dir.clone(), Duration::from_secs(1));

    let mut session = open(&config).expect("summon");
    session.wait_until_ready(Duration::from_secs(20)).unwrap();
    assert_eq!(session.probe().expect("first probe serves"), 3);
    let socket = session.socket().to_path_buf();

    // Hold the session but stay idle: the owner idle-reaps, unbinding the socket
    // and deleting its db — the session's connection is now dead.
    let reaped = common::wait_until(Duration::from_secs(15), || {
        !socket.exists() && common::owner_db_dirs(&runtime_dir) == 0
    });
    assert!(
        reaped,
        "the owner should idle-reap while the session is held idle"
    );

    // A request on the dead connection now fails (the owner is gone) — the bricked
    // state the MCP server must not be stuck in.
    assert!(
        session.probe().is_err(),
        "a request after the reap must fail — the held connection is dead"
    );

    // Recover: resummon a fresh owner and serve again on the SAME session handle.
    session
        .resummon(Duration::from_secs(20))
        .expect("resummon must re-establish a ready owner");
    assert_eq!(
        session.probe().expect("the post-resummon probe serves"),
        3,
        "the held session serves again after resummon — never bricked"
    );

    drop(session);
}

#[test]
fn second_open_connects_to_the_same_owner() {
    let (_vault_tmp, vault_root) = common::temp_vault(2);
    let rt_tmp = tempfile::TempDir::new().unwrap();
    let runtime_dir = rt_tmp.path().to_path_buf();

    // A modest TTL: long enough to stay up across both opens, short enough that
    // the owner self-reaps promptly after the test rather than lingering.
    let config = base_config(vault_root, runtime_dir, Duration::from_secs(3));

    let mut first = open(&config).expect("first summon");
    let pong1 = first.wait_until_ready(Duration::from_secs(20)).unwrap();

    // A second open finds the live owner (no new summon) — same pid.
    let mut second = open(&config).expect("second open");
    let pong2 = second.ping().expect("ping the live owner");
    assert_eq!(
        pong1.pid, pong2.pid,
        "the second open must connect to the already-running owner"
    );
    assert_eq!(pong2.serving, ServingState::Ready);
    assert_eq!(second.probe().unwrap(), 2);

    drop(first);
    drop(second);
}

/// NRN-360, the whole user story: summoning against a vault whose
/// `.norn/config.yaml` is present but invalid surfaces the config error on the
/// USER-error path ([`ClientError::Rejected`], carrying the oracle-shaped
/// `invalid config …` message) — never an owner-health/owner-gone failure or a
/// crash loop. The failed owner then EAGER-REAPS (well under the idle TTL, since
/// it can never serve a useful frame), so once the operator FIXES the config an
/// immediate re-summon spawns a fresh owner that re-reads it and serves reads.
#[test]
fn invalid_config_surfaces_error_eager_reaps_then_a_fix_is_picked_up() {
    let vault_tmp = tempfile::TempDir::new().unwrap();
    let vault_root = vault_tmp.path().join("vault");
    std::fs::create_dir_all(vault_root.join(".norn")).unwrap();
    // A well-formed-YAML but schema-invalid config (unknown top-level field).
    std::fs::write(vault_root.join(".norn/config.yaml"), "bogus: true\n").unwrap();
    std::fs::write(
        vault_root.join("a.md"),
        "---\ntype: note\ntitle: A\n---\nbody\n",
    )
    .unwrap();

    let rt_tmp = tempfile::TempDir::new().unwrap();
    let runtime_dir = rt_tmp.path().to_path_buf();
    // A deliberately LONG idle TTL: eager-reap must beat it by orders of
    // magnitude. If the owner instead lingered on its idle clock, the reap wait
    // below would time out long before 60s.
    let config = base_config(
        vault_root.clone(),
        runtime_dir.clone(),
        Duration::from_secs(60),
    );
    let socket = socket_path(&vault_root, &runtime_dir, &config.fingerprint);

    // The config error surfaces at whichever ping observes the failed warm-up
    // first — the `open` handshake or a `wait_until_ready` poll. Both must be a
    // Rejected, never OwnerGone (which would resummon into a crash loop).
    let err = match open(&config) {
        Ok(mut session) => session
            .wait_until_ready(Duration::from_secs(20))
            .expect_err("a bad-config owner must never reach ready"),
        Err(e) => e,
    };
    match &err {
        ClientError::Rejected { message, .. } => {
            assert!(
                message.contains("invalid config "),
                "expected the oracle config-error message, got {message:?}"
            );
            assert!(
                message.contains("unknown field `bogus`"),
                "expected the serde detail, got {message:?}"
            );
        }
        other => panic!("expected a Rejected config error, got {other:?}"),
    }

    // EAGER REAP: the failed owner tears down PROMPTLY — well under the 60s TTL —
    // removing its socket and db dir. (A lingering owner would time out here.)
    let reaped = common::wait_until(Duration::from_secs(20), || {
        !socket.exists() && common::owner_db_dirs(&runtime_dir) == 0
    });
    assert!(
        reaped,
        "the failed owner must eager-reap, not linger the idle TTL: socket exists={}, db dirs={}",
        socket.exists(),
        common::owner_db_dirs(&runtime_dir),
    );

    // Fix the config; because the stale-error owner reaped, an immediate
    // re-summon spawns a FRESH owner that re-reads the now-valid config and
    // serves reads — the fix is picked up at once, not after the TTL.
    std::fs::write(
        vault_root.join(".norn/config.yaml"),
        "links:\n  alias_field: aliases\n",
    )
    .unwrap();
    let mut session = open(&config).expect("re-summon after the config fix");
    let pong = session
        .wait_until_ready(Duration::from_secs(20))
        .expect("the fixed config must warm up to ready");
    assert_eq!(pong.serving, ServingState::Ready);
    assert_eq!(
        session.probe().expect("probe the fixed vault"),
        1,
        "the fixed owner should serve the one seeded note"
    );
    drop(session);
}

/// Finding 3 (reaper TOCTOU / drain): across repeated summon→probe→reap cycles,
/// every probe that gets a connection returns the EXACT document count — never a
/// partial, garbage, or mid-flight-dropped response. Alternating idle gaps drive
/// both same-owner reconnects (gap < TTL) and reap-then-resummon (gap > TTL).
#[test]
fn probes_across_reap_boundaries_are_never_corrupted() {
    let (_vault_tmp, vault_root) = common::temp_vault(4);
    let rt_tmp = tempfile::TempDir::new().unwrap();
    let ttl = Duration::from_millis(400);
    let config = base_config(vault_root, rt_tmp.path().to_path_buf(), ttl);

    for i in 0..10 {
        let mut session = open(&config).expect("summon-or-connect");
        session
            .wait_until_ready(Duration::from_secs(20))
            .expect("owner ready");
        assert_eq!(
            session
                .probe()
                .expect("probe must complete, not drop mid-flight"),
            4,
            "every probe must return the exact count"
        );
        drop(session);
        // Even iterations outlast the TTL (force a reap + resummon); odd ones do
        // not (reconnect to the live owner).
        let gap = if i % 2 == 0 {
            ttl + Duration::from_millis(300)
        } else {
            Duration::from_millis(50)
        };
        std::thread::sleep(gap);
    }
}
