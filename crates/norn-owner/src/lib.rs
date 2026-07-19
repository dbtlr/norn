#![forbid(unsafe_code)]
//! The summoned owner runtime: per-vault socket bind, owner-lifetime flock, one
//! born-with-owner cache slot, async control-plane + routed-read serve loop,
//! idle-TTL self-reap, exit-to-heal. The only crate that opens cache databases.
//!
//! May never: Parse argv or resolve names for CWD sugar (client concerns).
//!
//! # Entry from the `norn` bin
//!
//! The summoner (`norn-client`) spawns the current executable in *owner mode* —
//! `norn __norn-owner --socket <path> --vault-root <path> --ttl-secs <n>
//! [--build <fp>]`. That argv shape is an internal contract between the client
//! (which builds it) and this crate (which parses it); it is deliberately NOT a
//! clap subcommand, so it never touches the CLI grammar or the parity oracle.
//! The bin calls [`run_if_owner_mode`] before `norn_cli::run`; a non-owner argv
//! returns `None` and normal CLI dispatch proceeds.

#[cfg(unix)]
mod lifecycle;
#[cfg(unix)]
mod runtime;

#[cfg(unix)]
pub use runtime::{run, OwnerConfig};

use std::time::Duration;

/// One-line boundary contract, referenced by every dependent so each
/// declared edge in the crate map is compiler-load-bearing.
pub const CONTRACT: &str =
    "norn-owner: the summoned daemon runtime — the only crate that opens cache databases";

/// Direct-dependency contracts — the code reference that makes this
/// crate's declared edges load-bearing rather than manifest-only.
pub const DEP_CONTRACTS: &[&str] = &[norn_core::CONTRACT, norn_wire::CONTRACT];

/// The owner-mode sentinel: argv\[1\] when the client spawns us as an owner.
/// Underscore-prefixed so it reads as internal and can never collide with a
/// user-facing verb.
pub const OWNER_MODE_ARG: &str = "__norn-owner";

/// If this process was spawned in owner mode (argv\[1\] == [`OWNER_MODE_ARG`]),
/// parse the owner args, run the owner to completion, and return `Some(code)`.
/// Otherwise return `None` so normal CLI dispatch proceeds.
pub fn run_if_owner_mode() -> Option<i32> {
    let mut args = std::env::args();
    let _bin = args.next();
    if args.next().as_deref() != Some(OWNER_MODE_ARG) {
        return None;
    }
    let rest: Vec<String> = args.collect();
    Some(run_owner_from_args(&rest))
}

/// Parse the owner-mode args and run. Split out so it is unit-testable without a
/// real process spawn. On any parse failure, prints a diagnostic and returns a
/// non-zero code (the client's summon then observes the owner never bound and
/// surfaces an owner-health error).
#[cfg(unix)]
fn run_owner_from_args(args: &[String]) -> i32 {
    let parsed = match ParsedOwnerArgs::parse(args) {
        Ok(parsed) => parsed,
        Err(msg) => {
            eprintln!("norn owner: {msg}");
            return 2;
        }
    };
    match runtime::run(parsed.into_config()) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("norn owner: startup failed: {err}");
            1
        }
    }
}

#[cfg(not(unix))]
fn run_owner_from_args(_args: &[String]) -> i32 {
    eprintln!("norn owner: the summoned owner requires a Unix host (unix-domain sockets)");
    1
}

/// The parsed owner-mode argv, before it becomes an [`OwnerConfig`].
#[cfg(unix)]
struct ParsedOwnerArgs {
    socket: camino::Utf8PathBuf,
    vault_root: camino::Utf8PathBuf,
    ttl_secs: u64,
    build: Option<String>,
}

#[cfg(unix)]
impl ParsedOwnerArgs {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut socket: Option<camino::Utf8PathBuf> = None;
        let mut vault_root: Option<camino::Utf8PathBuf> = None;
        let mut ttl_secs: Option<u64> = None;
        let mut build: Option<String> = None;

        let mut it = args.iter();
        while let Some(flag) = it.next() {
            match flag.as_str() {
                "--socket" => {
                    socket = Some(next_value(&mut it, flag)?.into());
                }
                "--vault-root" => {
                    vault_root = Some(next_value(&mut it, flag)?.into());
                }
                "--ttl-secs" => {
                    let raw = next_value(&mut it, flag)?;
                    ttl_secs = Some(
                        raw.parse::<u64>()
                            .map_err(|_| format!("invalid --ttl-secs value: {raw}"))?,
                    );
                }
                "--build" => {
                    build = Some(next_value(&mut it, flag)?);
                }
                other => return Err(format!("unknown owner arg: {other}")),
            }
        }

        Ok(ParsedOwnerArgs {
            socket: socket.ok_or("missing --socket")?,
            vault_root: vault_root.ok_or("missing --vault-root")?,
            ttl_secs: ttl_secs.ok_or("missing --ttl-secs")?,
            build,
        })
    }

    fn into_config(self) -> OwnerConfig {
        OwnerConfig {
            socket_path: self.socket,
            vault_root: self.vault_root,
            idle_ttl: Duration::from_secs(self.ttl_secs),
            build: self.build,
        }
    }
}

#[cfg(unix)]
fn next_value<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<String, String> {
    it.next()
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_owner_argv() {
        let args = vec![
            "--socket".to_string(),
            "/run/norn/abc.def.sock".to_string(),
            "--vault-root".to_string(),
            "/vault".to_string(),
            "--ttl-secs".to_string(),
            "120".to_string(),
            "--build".to_string(),
            "deadbeef".to_string(),
        ];
        let parsed = ParsedOwnerArgs::parse(&args).unwrap();
        let config = parsed.into_config();
        assert_eq!(config.socket_path.as_str(), "/run/norn/abc.def.sock");
        assert_eq!(config.vault_root.as_str(), "/vault");
        assert_eq!(config.idle_ttl, Duration::from_secs(120));
        assert_eq!(config.build.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn build_is_optional() {
        let args = vec![
            "--socket".to_string(),
            "/s.sock".to_string(),
            "--vault-root".to_string(),
            "/v".to_string(),
            "--ttl-secs".to_string(),
            "5".to_string(),
        ];
        let parsed = ParsedOwnerArgs::parse(&args).unwrap();
        assert!(parsed.build.is_none());
    }

    #[test]
    fn missing_required_arg_errors() {
        let args = vec!["--socket".to_string(), "/s.sock".to_string()];
        assert!(ParsedOwnerArgs::parse(&args).is_err());
    }

    #[test]
    fn unknown_arg_errors() {
        let args = vec!["--bogus".to_string(), "x".to_string()];
        assert!(ParsedOwnerArgs::parse(&args).is_err());
    }
}
