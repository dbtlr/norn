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
//!
//! # Baked cache channel (`NORN_BAKED_CHANNEL`)
//!
//! Companion problem to the build fingerprint, same shape of fix: runtime
//! channel detection (`src/cache/channel.rs`) walks the executable's own
//! ancestors for a cargo `CACHEDIR.TAG`, which cannot survive the binary
//! being copied elsewhere — copying strips all build-tree ancestry. This
//! bakes a *default* channel into the binary at compile time instead, so
//! channel identity travels with the binary rather than living in its path
//! (NRN-285; see `channel.rs`'s module doc for where this slots into the
//! full resolution order).
//!
//! `NORN_BUILD_CHANNEL=live` / `=dev` bakes that value outright and is a hard
//! build error on any other non-empty value — an explicit override that
//! can't be honored is a bad invocation, not something to silently ignore.
//! Left unset, the auto rule bakes `dev` iff the tree being built is a `.git`
//! checkout (worktrees included — `.git` is a file there, not a directory)
//! that is neither a `cargo install --git` checkout (those build from a
//! cargo-managed clone under `CARGO_HOME`, which must stay `live`) nor a CI
//! release build (`CI` set — cargo-dist's GitHub Actions release artifacts
//! must also stay `live`). Anything else — notably a crates.io/tarball
//! source checkout with no `.git` at all — bakes nothing, leaving
//! `option_env!("NORN_BAKED_CHANNEL")` `None` and deferring entirely to
//! runtime detection.

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

    // Baked cache channel (see module docs): export it for
    // `option_env!("NORN_BAKED_CHANNEL")` in `src/cache/channel.rs`.
    emit_baked_channel(&manifest_dir);

    // `build.rs` is not part of the fingerprint (it produces no wire schema),
    // but its own edits must still re-run it. The per-file `src/` and
    // `Cargo.lock` lines are printed by `emit_build_id`.
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

/// Recursively collect every regular `.rs` file under `dir` into `out`.
///
/// The filter is what keeps the fingerprint reproducible across checkouts:
/// incidental non-source files (`.DS_Store`, editor swap/backup files) must
/// not perturb the id, and non-regular entries (fifos, sockets) must not
/// fail the read. Symlinks — file or directory — are skipped entirely: a
/// directory symlink could cycle the walk, and a symlinked module's target
/// wouldn't be rerun-tracked reliably anyway.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            collect_files(&path, out)?;
        } else if meta.is_file() && path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// Feed one length-prefixed `(name, contents)` record into the hasher.
///
/// Length prefixes (not separators) frame the records: contents may embed
/// any byte, including NUL, without two different trees ever producing the
/// same byte stream.
fn hash_record(hasher: &mut blake3::Hasher, name: &str, contents: &[u8]) {
    hasher.update(&(name.len() as u64).to_le_bytes());
    hasher.update(name.as_bytes());
    hasher.update(&(contents.len() as u64).to_le_bytes());
    hasher.update(contents);
}

/// Hash the crate's `src/` tree (sorted relative paths + contents of every
/// regular `.rs` file) plus `Cargo.toml` and `Cargo.lock`, export the digest
/// as `NORN_BUILD_ID`, and print the `rerun-if-changed` lines that keep the
/// id — and the completion/man side effects — tracked (see module docs).
///
/// `Cargo.toml` is included because a feature or profile flip can change the
/// wire schema without touching `src/` or the lockfile; `Cargo.lock` because
/// a dep-tree change can. Both are REQUIRED: a build without a readable
/// lockfile would silently drop the dep-tree contribution and let two
/// wire-different builds share an id, so it fails the build instead.
fn emit_build_id(manifest_dir: &Path) -> std::io::Result<()> {
    let src_dir = manifest_dir.join("src");
    let mut files = Vec::new();
    collect_files(&src_dir, &mut files)?;
    // Absolute paths all share the manifest-dir prefix, so a plain sort
    // orders identically to sorting the relative paths; the RELATIVE path is
    // what feeds the hash, keeping the id stable across checkout locations.
    files.sort();

    let mut hasher = blake3::Hasher::new();
    for file in &files {
        let rel = file.strip_prefix(manifest_dir).unwrap_or(file);
        hash_record(&mut hasher, &rel.to_string_lossy(), &std::fs::read(file)?);
        println!("cargo:rerun-if-changed={}", file.display());
    }

    for name in ["Cargo.toml", "Cargo.lock"] {
        let path = manifest_dir.join(name);
        let contents = std::fs::read(&path).map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!("NORN_BUILD_ID requires a readable {name}: {e}"),
            )
        })?;
        hash_record(&mut hasher, name, &contents);
        println!("cargo:rerun-if-changed={}", path.display());
    }

    println!(
        "cargo:rustc-env=NORN_BUILD_ID={}",
        hasher.finalize().to_hex()
    );
    // The dir line catches file additions/removals; per-file lines above are
    // the reliable rerun trigger for edits.
    println!("cargo:rerun-if-changed={}", src_dir.display());
    Ok(())
}

/// Read `NORN_BUILD_CHANNEL` and `CARGO_HOME`/`CI`, decide whether to bake a
/// channel (see module docs), and either emit `NORN_BAKED_CHANNEL` or panic
/// on an invalid override. The decision itself lives in [`bake_channel`],
/// a pure function of these inputs; this is only the imperative shell that
/// gathers them from the real environment.
fn emit_baked_channel(manifest_dir: &Path) {
    println!("cargo:rerun-if-env-changed=NORN_BUILD_CHANNEL");
    println!("cargo:rerun-if-env-changed=CI");

    let build_channel_env = env::var("NORN_BUILD_CHANNEL").ok();
    let has_git_entry = manifest_dir.join(".git").symlink_metadata().is_ok();
    // `cargo install --git` builds from a clone under `CARGO_HOME` (default
    // `$HOME/.cargo`) — that clone has a `.git` dir too, so it must be
    // excluded here or it would wrongly auto-bake `dev`.
    let cargo_home = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")));
    let under_cargo_home = cargo_home.is_some_and(|home| manifest_dir.starts_with(home));
    let ci_set = env::var("CI").is_ok_and(|v| !v.is_empty());

    match bake_channel(
        build_channel_env.as_deref(),
        has_git_entry,
        under_cargo_home,
        ci_set,
    ) {
        Ok(Some(channel)) => println!("cargo:rustc-env=NORN_BAKED_CHANNEL={channel}"),
        // Nothing baked: emit nothing, so `option_env!` sees `None` and
        // runtime detection (`src/cache/channel.rs`) decides instead.
        Ok(None) => {}
        Err(value) => panic!(
            "NORN_BUILD_CHANNEL must be exactly \"live\" or \"dev\" (or unset); got {value:?}"
        ),
    }
}

/// Pure decision behind [`emit_baked_channel`]: which channel (if any) to
/// bake, given the `NORN_BUILD_CHANNEL` override and the three auto-rule
/// facts about the checkout being built. Factored out so the environment/
/// filesystem reads stay in the imperative shell above.
///
/// - `Some("live"|"dev")` from the override bakes that value outright.
/// - Any other non-empty override value is `Err` (the offending value,
///   verbatim, for the panic message) — an override that can't be honored is
///   a bad invocation, not something to paper over.
/// - Unset/empty override falls to the auto rule: `dev` iff the checkout has
///   a `.git` entry, is not under `CARGO_HOME`, and `CI` is unset — else
///   nothing is baked (deferring to runtime detection).
fn bake_channel(
    build_channel_env: Option<&str>,
    has_git_entry: bool,
    under_cargo_home: bool,
    ci_set: bool,
) -> Result<Option<&'static str>, String> {
    match build_channel_env {
        Some("live") => return Ok(Some("live")),
        Some("dev") => return Ok(Some("dev")),
        Some("") | None => {}
        Some(other) => return Err(other.to_string()),
    }
    if has_git_entry && !under_cargo_home && !ci_set {
        Ok(Some("dev"))
    } else {
        Ok(None)
    }
}
