//! Shared test-support helpers. The single home for the skip-if-absent
//! oracle probe consumed by this crate's and `norn-parity`'s test suites, and
//! for the isolated environment anything here spawns the oracle under.
//!
//! This is the one place in `norn-fixtures` that shells out to `norn`: the
//! generator itself never does (it produces inputs independent of the system
//! under test). The probe lives here only so both crates' tests share one
//! copy rather than re-deriving it.

use std::path::PathBuf;
use std::process::Command;

use tempfile::TempDir;

/// The longest `sun_path` an `AF_UNIX` socket may carry: 104 bytes on macOS,
/// 108 on Linux. The smaller of the two is the portable bound.
const SUN_PATH_MAX: usize = 104;

/// What a caller must leave for the socket file a binary creates INSIDE the
/// runtime dir: a `norn/` subdirectory plus a fixed 38-character socket name,
/// plus the terminating NUL.
const RUNTIME_DIR_RESERVE: usize = 6 + 38 + 1;

/// A short-lived runtime directory whose path is short enough to hold an
/// `AF_UNIX` socket.
///
/// `std::env::temp_dir()` is NOT usable for this on macOS, where it is a
/// per-user `/var/folders/<2>/<26>/T/` path some 50 characters long: add a
/// temp-dir name, `norn/`, and a 38-character socket name and the result
/// overruns `sun_path`. A binary that then summons a vault owner reports only
/// that the owner "never became reachable", after a retry window — silence
/// and a stall, not a diagnosis. `/tmp` is short and POSIX-guaranteed, so it
/// is preferred, with the platform temp dir as a fallback and an explicit
/// length check so a path that cannot work fails HERE, loudly.
pub fn short_runtime_dir(prefix: &str) -> std::io::Result<TempDir> {
    let base = ["/tmp", "/var/tmp"]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let dir = TempDir::with_prefix_in(prefix, base)?;
    let len = dir.path().as_os_str().len();
    if len + RUNTIME_DIR_RESERVE > SUN_PATH_MAX {
        return Err(std::io::Error::other(format!(
            "runtime dir {} is {len} bytes; a socket under it would overrun the {SUN_PATH_MAX}-byte \
             AF_UNIX sun_path limit",
            dir.path().display()
        )));
    }
    Ok(dir)
}

/// A cleared environment for spawning the oracle, and the temp tree backing
/// it — dropped together.
///
/// Inheriting the caller's environment makes the host an input to an
/// assertion about the ORACLE: a `norn serve` daemon reachable through `$HOME`
/// answers instead of the binary under test, a `NORN_ROOT` points it at
/// another vault, and a non-UTF-8 locale changes what it renders. The
/// environment is cleared and rebuilt from PATH + TMPDIR, a pinned UTF-8
/// locale, and a scratch HOME/XDG tree — the same shape `norn-parity`'s
/// runner spawns under (`norn_parity::exec::SpawnEnv`), kept minimal here
/// because this crate only needs to run one read-only verb.
pub struct ScratchEnv {
    dir: TempDir,
    home: PathBuf,
}

impl ScratchEnv {
    pub fn new() -> std::io::Result<ScratchEnv> {
        let dir = short_runtime_dir("norn-fixtures-")?;
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home)?;
        Ok(ScratchEnv { dir, home })
    }

    /// `program` with this environment applied. `XDG_RUNTIME_DIR` is the
    /// temp root itself — see [`short_runtime_dir`] for why its path is
    /// chosen the way it is.
    pub fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command.env_clear();
        for passthrough in ["PATH", "TMPDIR"] {
            if let Some(value) = std::env::var_os(passthrough) {
                command.env(passthrough, value);
            }
        }
        command.env("LC_ALL", "C.UTF-8");
        command.env_remove("LANG");
        command.env_remove("LC_CTYPE");
        command.env("HOME", &self.home);
        command.env("XDG_CACHE_HOME", self.dir.path().join("cache"));
        command.env("XDG_CONFIG_HOME", self.dir.path().join("config"));
        command.env("XDG_RUNTIME_DIR", self.dir.path());
        command.env_remove("NORN_ROOT");
        command.env_remove("NORN_CONFIG_DIR");
        command
    }
}

/// `true` when the pinned oracle (`norn`, ADR 0018) is on PATH and its
/// `--version` succeeds. Oracle-touching tests skip cleanly when it is
/// absent (it is installed before `cargo test` in CI).
pub fn oracle_present() -> bool {
    let Ok(env) = ScratchEnv::new() else {
        return false;
    };
    env.command("norn")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
