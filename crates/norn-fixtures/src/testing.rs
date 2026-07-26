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

/// The hex-encoded vault-root hash and build fingerprint an owner socket is
/// named from — `<root>.<build>.sock`.
const ROOT_HASH_HEX_LEN: usize = 16;
const FINGERPRINT_HEX_LEN: usize = 16;
const SOCKET_SUFFIX: &str = ".sock";

/// The socket file name a summoned owner binds inside the runtime dir,
/// derived rather than pinned as a literal so a change to either hash width
/// (or the suffix) moves the reserve with it instead of quietly staling it.
const SOCKET_NAME_LEN: usize = ROOT_HASH_HEX_LEN + 1 + FINGERPRINT_HEX_LEN + SOCKET_SUFFIX.len();

/// What a caller must leave INSIDE the runtime dir for that socket: the
/// separator before the `norn` subdirectory, the subdirectory itself, the
/// separator after it, the socket name, and the terminating NUL.
const RUNTIME_DIR_RESERVE: usize = 1 + "norn".len() + 1 + SOCKET_NAME_LEN + 1;

/// A short-lived runtime directory whose path is short enough to hold an
/// `AF_UNIX` socket.
///
/// `std::env::temp_dir()` is NOT usable for this on macOS, where it is a
/// per-user `/var/folders/<2>/<26>/T/` path some 50 characters long: add a
/// temp-dir name, `norn/`, and the socket name and the result overruns
/// `sun_path`. A binary that then summons a vault owner reports only that the
/// owner "never became reachable", after a retry window — silence and a
/// stall, not a diagnosis.
///
/// Bases are tried shortest-first by ACTUALLY CREATING the directory, so a
/// base that exists but cannot be written to (a read-only `/tmp`, a
/// restrictive mount) falls through to the next rather than being selected on
/// its existence alone. The length check then rejects a path that could not
/// hold a socket even though it was created — the platform temp dir on macOS
/// being exactly that — so both failure modes surface HERE, loudly, instead
/// of as a stall much later.
pub fn short_runtime_dir(prefix: &str) -> std::io::Result<TempDir> {
    let bases = [
        PathBuf::from("/tmp"),
        PathBuf::from("/var/tmp"),
        std::env::temp_dir(),
    ];
    let dir = bases
        .iter()
        .find_map(|base| TempDir::with_prefix_in(prefix, base).ok())
        .ok_or_else(|| {
            std::io::Error::other(format!(
                "no writable base for a runtime dir (tried {})",
                bases
                    .iter()
                    .map(|b| b.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
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
/// locale, and a scratch HOME/XDG tree — close to the shape `norn-parity`'s
/// runner spawns under (`norn_parity::exec::SpawnEnv`), kept minimal here
/// because this crate only spawns the pinned 0.48.x oracle to run one
/// read-only verb. It does not set `NORN_EPHEMERAL_TTL_SECS`: that oracle has
/// no ephemeral-owner tier and no reader for the var, so there is nothing
/// here for it to control.
pub struct ScratchEnv {
    dir: TempDir,
    home: PathBuf,
    cache: PathBuf,
    config: PathBuf,
}

impl ScratchEnv {
    /// Every directory handed to a spawned binary EXISTS before it runs.
    /// Pointing `HOME` / `XDG_CACHE_HOME` / `XDG_CONFIG_HOME` at a path that
    /// merely lies inside the temp root is not enough: an XDG consumer is
    /// entitled to assume its base directory is already there and need not
    /// create parents, so a missing one surfaces as a write failure from the
    /// binary under test — reading as its behavior rather than as the
    /// harness's setup.
    pub fn new() -> std::io::Result<ScratchEnv> {
        let dir = short_runtime_dir("norn-fixtures-")?;
        let home = dir.path().join("home");
        let cache = dir.path().join("cache");
        let config = dir.path().join("config");
        for base in [&home, &cache, &config] {
            std::fs::create_dir_all(base)?;
        }
        Ok(ScratchEnv {
            dir,
            home,
            cache,
            config,
        })
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
        command.env("XDG_CACHE_HOME", &self.cache);
        command.env("XDG_CONFIG_HOME", &self.config);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_runtime_dir_leaves_room_for_the_socket() {
        let dir = short_runtime_dir("norn-testing-").expect("no usable runtime-dir base");
        let len = dir.path().as_os_str().len();
        assert!(
            len + RUNTIME_DIR_RESERVE <= SUN_PATH_MAX,
            "{} is {len} bytes, leaving no room for a {RUNTIME_DIR_RESERVE}-byte socket path \
             under the {SUN_PATH_MAX}-byte limit",
            dir.path().display()
        );
    }

    #[test]
    fn the_reserve_matches_the_socket_path_an_owner_actually_binds() {
        // The shape observed from a summoned owner's argv:
        // `<runtime dir>/norn/<root>.<build>.sock`.
        let name = "619028d8e5cd54fe.c709f55a3c8ec4d1.sock";
        assert_eq!(
            name.len(),
            SOCKET_NAME_LEN,
            "the derived socket-name length no longer matches the name an owner binds — \
             update ROOT_HASH_HEX_LEN / FINGERPRINT_HEX_LEN / SOCKET_SUFFIX to match"
        );
        assert_eq!(
            RUNTIME_DIR_RESERVE,
            format!("/norn/{name}").len() + 1,
            "the reserve is the separator, the `norn` subdirectory, the socket name, and the NUL"
        );
    }

    #[test]
    fn a_scratch_env_clears_the_caller_environment() {
        let env = ScratchEnv::new().expect("failed to create the scratch environment");
        let command = env.command("true");
        let vars: std::collections::BTreeMap<_, _> = command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().to_string(),
                    v.map(|v| v.to_string_lossy().to_string()),
                )
            })
            .collect();
        assert_eq!(
            vars.get("LC_ALL").map(Option::as_deref),
            Some(Some("C.UTF-8")),
            "the locale is pinned, not forwarded"
        );
        for removed in ["LANG", "LC_CTYPE", "NORN_ROOT", "NORN_CONFIG_DIR"] {
            // `env_clear` drops it and `env_remove` keeps it dropped: either
            // way the child must never see a value for it.
            assert!(
                !matches!(vars.get(removed), Some(Some(_))),
                "{removed} must not reach a spawned binary, got {:?}",
                vars.get(removed)
            );
        }
        let set: std::collections::BTreeSet<&str> = vars
            .iter()
            .filter(|(_, v)| v.is_some())
            .map(|(k, _)| k.as_str())
            .collect();
        for key in &set {
            assert!(
                [
                    "PATH",
                    "TMPDIR",
                    "LC_ALL",
                    "HOME",
                    "XDG_CACHE_HOME",
                    "XDG_CONFIG_HOME",
                    "XDG_RUNTIME_DIR"
                ]
                .contains(key),
                "{key} is set for the child but is not on the allowlist"
            );
        }
        assert!(
            vars.get("HOME")
                .is_some_and(|v| v.as_deref().is_some_and(|h| h.ends_with("home"))),
            "HOME points into the scratch tree"
        );
        for base in [
            "HOME",
            "XDG_CACHE_HOME",
            "XDG_CONFIG_HOME",
            "XDG_RUNTIME_DIR",
        ] {
            let path = vars
                .get(base)
                .and_then(|v| v.as_deref())
                .unwrap_or_else(|| panic!("{base} is set for the child"));
            assert!(
                std::path::Path::new(path).is_dir(),
                "{base} must EXIST before a binary runs, not just be a path under the temp root"
            );
        }
    }
}
