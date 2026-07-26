//! NRN-502: `NORN_EPHEMERAL_TTL_SECS` (`EPHEMERAL_TTL_ENV`), read by
//! `ephemeral_idle_ttl`, actually reaches a REAL summoned owner and controls
//! how promptly it self-reaps. This is the mechanism the `norn-parity`
//! harness relies on to bound owner lingering across a gated run of the
//! full case suite: a read case shares one owner per (fixture, side), while
//! only a mutating case gets its own per-case owner — either way, the cost
//! is meant to be lingering, not spawning.
//!
//! Hermetic like `summon_lifecycle.rs`: TempDir vault, TempDir runtime dir,
//! bounded waits, never sleeps-as-synchronization. A single test in this
//! binary: it mutates the process-global env var, so (per
//! `warmup_shutdown.rs`'s same reasoning) it must not race another test doing
//! the same.

mod common;

use std::time::Duration;

use common::base_config;
use norn_client::{ephemeral_idle_ttl, open, SummonConfig, EPHEMERAL_TTL_ENV};

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

    // Pin the harness seam `for_vault` actually depends on: the env override
    // must reach a config built the production way, not just the raw
    // `ephemeral_idle_ttl()` read above.
    assert_eq!(
        SummonConfig::for_vault(vault_root.clone(), common::norn_bin())
            .expect("for_vault should succeed")
            .idle_ttl,
        Duration::from_secs(1),
        "SummonConfig::for_vault must join the env-derived TTL, not the 120s default"
    );

    let config = base_config(vault_root, runtime_dir.clone(), ttl);

    let mut session = open(&config).expect("summon-or-connect should succeed");
    let socket = session.socket().to_path_buf();
    session
        .wait_until_ready(Duration::from_secs(20))
        .expect("owner should warm up to ready");
    assert_eq!(session.probe().expect("probe should serve"), 2);

    // The born-with-owner db exists while the owner serves — pins the reap
    // assertion below to an actual create-then-remove transition rather than
    // letting it false-pass on a db dir that was simply never created.
    assert_eq!(
        common::owner_db_dirs(&runtime_dir),
        1,
        "the owner's db dir should exist while it serves"
    );

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
