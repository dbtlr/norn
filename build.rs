//! Generates shell completion scripts and the roff man page as side effects
//! of building `norn`, and emits the build fingerprint the routing handshake
//! gates on (NRN-247). The completion/man outputs land under the workspace
//! `target/` directory so cargo-dist's `include` directive (in
//! `dist-workspace.toml`) can pick them up without requiring a separate
//! `just completions` / `just manpage` step in the release pipeline.
//!
//! The CLI surface is reused via `#[path = "src/cli.rs"]` so this script
//! tracks the real `clap` definitions automatically. `cli.rs` is kept free
//! of intra-crate dependencies (see commit history) to make the include
//! trick viable — except `crate::standards::parse_duration`, satisfied here
//! by including the self-contained `src/standards/duration.rs` as a module
//! named `standards`.
//!
//! # Build fingerprint (`NORN_BUILD_ID`)
//!
//! The handshake requires an exact build match, not just an exact
//! `CARGO_PKG_VERSION` match: two builds of the same `0.x` version can carry
//! different wire schemas (an additive `ApplyReport` field a stale daemon
//! renders as zeros). This script hashes the crate's `src/` tree (sorted
//! relative paths + file contents) plus `Cargo.lock` with blake3 and exports
//! the hex digest as `NORN_BUILD_ID`. It is a **source-content** id, not a
//! timestamp: an unchanged rebuild reproduces the same id (so it keeps
//! routing), while any source or lockfile change mints a new one (so a
//! same-version dev rebuild is detected). Relative paths — never absolute —
//! feed the hash, so the id is reproducible across checkouts and the release
//! matrix.
//!
//! **Rerun surface.** Cargo narrows `rerun-if-changed` tracking to exactly the
//! directives a build script prints. This script prints one per hashed file
//! (so any source edit re-runs it and re-mints the id — this subsumes the old
//! explicit `cli.rs` / `duration.rs` lines, which stay covered because they
//! live under `src/`), plus the `src/` dir and `Cargo.lock` so additions,
//! removals, and dep changes are caught too. A no-change rebuild touches none
//! of these, so it neither re-runs this script nor changes the id.

use std::env;
use std::path::{Path, PathBuf};

use clap::CommandFactory;
use clap_complete::{generate_to, Shell};
use clap_complete_nushell::Nushell;
use clap_mangen::Man;

#[path = "src/cli.rs"]
#[allow(dead_code)]
mod cli;

// Resolves cli.rs's `crate::standards::parse_duration` (the `--retention`
// value parser) inside the build-script crate.
#[path = "src/standards/duration.rs"]
#[allow(dead_code)]
mod standards;

fn main() -> std::io::Result<()> {
    // CARGO_MANIFEST_DIR is the repo root, so cargo-dist's `include` entries
    // and the build-script outputs share the same base directory.
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR")
            .expect("CARGO_MANIFEST_DIR must be set by cargo when running build.rs"),
    );

    let completions_dir = manifest_dir.join("target").join("completions");
    let man_dir = manifest_dir.join("target").join("man");

    std::fs::create_dir_all(&completions_dir)?;
    std::fs::create_dir_all(&man_dir)?;

    let mut cmd = cli::Cli::command();
    generate_to(Shell::Bash, &mut cmd, "norn", &completions_dir)?;
    generate_to(Shell::Zsh, &mut cmd, "norn", &completions_dir)?;
    generate_to(Shell::Fish, &mut cmd, "norn", &completions_dir)?;
    generate_to(Nushell, &mut cmd, "norn", &completions_dir)?;

    let man = Man::new(cmd);
    let mut buffer = Vec::new();
    man.render(&mut buffer)?;
    std::fs::write(man_dir.join("norn.1"), buffer)?;

    // Build fingerprint: hash the sorted src/ tree + Cargo.lock (see module
    // docs) and export it for `env!("NORN_BUILD_ID")` in the daemon pong and
    // the routing gate.
    emit_build_id(&manifest_dir)?;

    // `build.rs` is not part of the fingerprint (it produces no wire schema),
    // but its own edits must still re-run it. The per-file `src/` and
    // `Cargo.lock` lines are printed by `emit_build_id`.
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

/// Recursively collect every file under `dir` into `out`.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_files(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

/// Hash the crate's `src/` tree (sorted relative paths + contents) and
/// `Cargo.lock`, export the digest as `NORN_BUILD_ID`, and print the
/// `rerun-if-changed` lines that keep the id — and the completion/man side
/// effects — tracked (see module docs).
fn emit_build_id(manifest_dir: &Path) -> std::io::Result<()> {
    let src_dir = manifest_dir.join("src");
    let mut files = Vec::new();
    collect_files(&src_dir, &mut files)?;
    // Sort by the RELATIVE path so the hash is stable regardless of the
    // absolute checkout location or filesystem read order.
    files.sort_by(|a, b| {
        let ra = a.strip_prefix(manifest_dir).unwrap_or(a);
        let rb = b.strip_prefix(manifest_dir).unwrap_or(b);
        ra.cmp(rb)
    });

    let mut hasher = blake3::Hasher::new();
    for file in &files {
        let rel = file.strip_prefix(manifest_dir).unwrap_or(file);
        // Path then a NUL separator then contents: a rename or a content move
        // across files can't collide with an unchanged tree.
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update(&[0]);
        hasher.update(&std::fs::read(file)?);
        println!("cargo:rerun-if-changed={}", file.display());
    }

    // Cargo.lock: a dep-tree change can shift the wire schema without touching
    // src/, so it belongs in the fingerprint.
    let lock = manifest_dir.join("Cargo.lock");
    if let Ok(contents) = std::fs::read(&lock) {
        hasher.update(b"Cargo.lock");
        hasher.update(&[0]);
        hasher.update(&contents);
    }

    println!(
        "cargo:rustc-env=NORN_BUILD_ID={}",
        hasher.finalize().to_hex()
    );
    // The dir line catches file additions/removals; the Cargo.lock line catches
    // dep changes. Per-file lines above are the reliable rerun trigger for edits.
    println!("cargo:rerun-if-changed={}", src_dir.display());
    println!("cargo:rerun-if-changed={}", lock.display());
    Ok(())
}
