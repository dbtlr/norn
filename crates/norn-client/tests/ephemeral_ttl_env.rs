//! NRN-502: `NORN_EPHEMERAL_TTL_SECS` (`EPHEMERAL_TTL_ENV`), read by
//! `ephemeral_idle_ttl`, actually reaches a REAL summoned owner and controls
//! how promptly it self-reaps. This is the mechanism the `norn-parity`
//! harness relies on to keep one owner per per-case fixture vault from
//! lingering the 120s production default across a gated run of hundreds of
//! cases — the cost is meant to be lingering, not spawning.
//!
//! Hermetic like `summon_lifecycle.rs`: TempDir vault, TempDir runtime dir,
//! bounded waits, never sleeps-as-synchronization. A single test in this
//! binary: it mutates the process-global env var, so (per
//! `warmup_shutdown.rs`'s same reasoning) it must not race another test doing
//! the same.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use norn_client::{ephemeral_idle_ttl, open, SummonConfig, EPHEMERAL_TTL_ENV};

fn base_config(vault_root: PathBuf, runtime_dir: PathBuf, ttl: Duration) -> SummonConfig {
    SummonConfig {
        vault_root,
        runtime_dir,
        fingerprint: "envttlfingerp01".to_string(),
        idle_ttl: ttl,
        owner_exe: common::norn_bin(),
        connect_budget: Duration::from_secs(15),
        config_override: None,
        events_dir: None,
    }
}

#[test]
fn env_override_shortens_a_real_owners_idle_ttl() {
    let (_vault_tmp, vault_root) = common::temp_vault(2);
    let rt_tmp = tempfile::TempDir::new().unwrap();
    let runtime_dir = rt_tmp.path().to_path_buf();

    // Set the override BEFORE reading it — proves the read, not just a
    // hand-passed short Duration (which every other summon test already
    // uses to keep its own owners from lingering).
    std::env::set_var(EPHEMERAL_TTL_ENV, "1");
    let ttl = ephemeral_idle_ttl();
    assert_eq!(
        ttl,
        Duration::from_secs(1),
        "ephemeral_idle_ttl must read the override, not fall back to the 120s default"
    );

    let config = base_config(vault_root, runtime_dir.clone(), ttl);

    let mut session = open(&config).expect("summon-or-connect should succeed");
    let socket = session.socket().to_path_buf();
    session
        .wait_until_ready(Duration::from_secs(20))
        .expect("owner should warm up to ready");
    assert_eq!(session.probe().expect("probe should serve"), 2);

    // Drop the session so nothing keeps the owner busy; the env-derived 1s
    // idle TTL then fires.
    drop(session);
    std::env::remove_var(EPHEMERAL_TTL_ENV);

    // Bounded well under the 120s production default: a bare reap could
    // pass at any TTL, but only the env-overridden 1s TTL clears this bound.
    let reaped = common::wait_until(Duration::from_secs(15), || {
        !socket.exists() && common::owner_db_dirs(&runtime_dir) == 0
    });
    assert!(
        reaped,
        "owner summoned under the env-overridden TTL should idle-reap promptly: \
         socket exists={}, db dirs={}",
        socket.exists(),
        common::owner_db_dirs(&runtime_dir),
    );
}
