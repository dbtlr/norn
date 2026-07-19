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

use norn_client::{open, SummonConfig};
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
