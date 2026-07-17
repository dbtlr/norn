//! NRN-285: cache channel identity must travel WITH the binary, not live in
//! its path.
//!
//! Incident: a benchmark agent copied `target/release/norn` to `/tmp/norn-after`
//! and ran it against a real vault. `/tmp` has no `CACHEDIR.TAG` ancestor, so
//! path-based runtime detection alone resolved `live` and the schema-5 dev
//! binary silently migrated the live cache, locking out the installed older
//! binary. This test reproduces that incident shape exactly — copy the test
//! binary into an isolated temp dir and run it from there — and is the
//! standing tripwire for the incident class: it must always report `dev`.
//!
//! Either of the two new layers is independently sufficient to pass this:
//! locally, this checkout builds with a `.git` entry, outside `CARGO_HOME`,
//! usually with no `CI` set, so `build.rs` bakes `NORN_BAKED_CHANNEL=dev` into
//! the test binary regardless of where the copy later runs. In CI (`CI` set),
//! the auto rule bakes nothing, so the runtime temp-dir heuristic in
//! `src/cache/channel.rs` is what catches it instead. A binary with neither
//! protection would resolve `live` here and fail this test.

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

/// Pre-write a FRESH lazy-sweep throttle marker (`<cache_home>/norn/.last-prune`)
/// so norn invocations under this cache home never spawn a detached GC sweep
/// child (NRN-287) that could race this test. Mirrors src/cache/prune.rs
/// `PRUNE_MARKER`.
fn prewrite_prune_marker(cache_home: &Path) {
    let tree = cache_home.join("norn");
    let _ = std::fs::create_dir_all(&tree);
    let _ = std::fs::write(tree.join(".last-prune"), b"");
}

#[test]
fn binary_copied_out_of_build_tree_resolves_dev_channel() {
    let bin_src = Path::new(env!("CARGO_BIN_EXE_norn"));
    let copy_dir = TempDir::new().expect("temp dir for copied binary");
    let bin_dst = copy_dir.path().join(bin_src.file_name().unwrap());
    std::fs::copy(bin_src, &bin_dst).expect("copy norn binary out of its build tree");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&bin_dst).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin_dst, perms).unwrap();
    }

    // Isolated XDG dirs so this never touches the real ~/.cache.
    let xdg = TempDir::new().expect("temp dir for XDG isolation");
    let cache_home = xdg.path().join("cache");
    let state_home = xdg.path().join("state");

    let vault = TempDir::new().expect("temp dir for fixture vault");
    std::fs::write(vault.path().join("a.md"), "---\ntype: note\n---\nbody\n").unwrap();

    prewrite_prune_marker(&cache_home);

    let out = Command::new(&bin_dst)
        .env_remove("NORN_CACHE_CHANNEL")
        .env("XDG_CACHE_HOME", &cache_home)
        .env("XDG_STATE_HOME", &state_home)
        .arg("--cwd")
        .arg(vault.path())
        .args(["cache", "status", "--format", "json"])
        .output()
        .expect("run copied norn binary");

    assert!(
        out.status.success(),
        "cache status failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid json");
    assert_eq!(
        v["channel"], "dev",
        "a binary copied out of its build tree must never resolve to the live \
         channel (NRN-285): {v}"
    );
}
