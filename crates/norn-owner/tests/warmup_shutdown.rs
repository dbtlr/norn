//! Finding 2: a shutdown (here an idle-reap) that lands mid-warm-up must await
//! the build to completion before tearing down the born-with-owner db dir — the
//! `full_build` cannot be aborted mid-block, so awaiting is the only bound that
//! prevents orphaned temp files defeating the disposable-derivation cleanup.
//!
//! Hermetic: a TempDir vault + a TempDir runtime dir, a short idle TTL, and the
//! internal `NORN_OWNER_WARMUP_DELAY_MS` slow-build seam. This is the only test
//! in this binary, so the process-global env var it sets cannot race another.

#[cfg(unix)]
#[test]
fn shutdown_during_warmup_awaits_the_build_and_leaves_no_orphans() {
    use camino::Utf8PathBuf;
    use std::time::{Duration, Instant};

    let rt = tempfile::TempDir::new().unwrap();
    let runtime_dir = Utf8PathBuf::from_path_buf(rt.path().to_path_buf()).unwrap();

    let vault_tmp = tempfile::TempDir::new().unwrap();
    let vault_root = Utf8PathBuf::from_path_buf(vault_tmp.path().to_path_buf()).unwrap();
    std::fs::write(
        vault_root.join("a.md").as_std_path(),
        "---\ntype: note\n---\n",
    )
    .unwrap();

    // Slow warm-up (600ms) but a short idle TTL (150ms): the reaper requests
    // shutdown while the build is still in flight.
    std::env::set_var("NORN_OWNER_WARMUP_DELAY_MS", "600");
    let config = norn_owner::OwnerConfig {
        socket_path: runtime_dir.join("h.fp.sock"),
        vault_root,
        idle_ttl: Duration::from_millis(150),
        build: None,
        config_path: None,
        events_dir: None,
    };

    let start = Instant::now();
    let code = norn_owner::run(config).expect("owner run should succeed");
    let elapsed = start.elapsed();
    std::env::remove_var("NORN_OWNER_WARMUP_DELAY_MS");

    assert_eq!(code, 0, "an idle reap is a clean exit");
    // Proof the await happened: run() cannot return before the delayed build.
    assert!(
        elapsed >= Duration::from_millis(600),
        "run() returned before warm-up finished ({elapsed:?}) — the build was not awaited"
    );

    // No orphaned owner db dirs — the disposable derivation was fully cleaned.
    let orphans = std::fs::read_dir(rt.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("norn-owner-db-")
        })
        .count();
    assert_eq!(
        orphans, 0,
        "warm-up build orphaned temp files in the runtime dir"
    );
}
