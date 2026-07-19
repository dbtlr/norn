//! The routed-read entry the read verbs share: resolve the target vault,
//! summon-or-connect its owner, and wait for it to be ready.
//!
//! This is the CLI's whole job on the request side of a read (ADR 0016/0017):
//! turn the global flags into a resolved vault, hand it to `norn-client` (the
//! only crate that spawns owners or dials sockets), and return a live
//! [`OwnerSession`] the verb sends its `Params` to.

use std::time::Duration;

use norn_client::{
    open, ClientError, ConfigHome, OwnerSession, Registry, ResolveInput, SummonConfig,
};
use norn_config::ConfigError;

use crate::cli::GlobalArgs;
use crate::display::Diagnostic;

/// How long a read waits for a freshly-summoned owner to finish warming up
/// before giving up. Warm-up is size-linear (a one-shot full cache build), so
/// this is a generous ceiling, not an expected latency: agent bursts and small
/// vaults are ready in well under a second, while a very large vault's first
/// summon still completes inside the budget. The owner answers pings promptly
/// throughout warm-up, so a truly hung owner is caught by the per-request stall
/// budget long before this ceiling — this only bounds a legitimately slow build.
pub const MAX_WAIT: Duration = Duration::from_secs(120);

/// Resolve the target vault from the global flags, summon-or-connect its owner,
/// and return a ready session. On failure returns a soft-landing [`Diagnostic`]
/// the caller presents on stderr (NRN-361): the headline plus a next-step hint
/// where a diagnosis path genuinely exists (unknown vault name → `vault list`,
/// a stale registry entry → re-register, and so on).
pub fn open_session(global: &GlobalArgs) -> Result<OwnerSession, Diagnostic> {
    let cwd = std::env::current_dir()
        .map_err(|e| Diagnostic::new(format!("cannot read the current directory: {e}")))?;
    let home = ConfigHome::from_env().map_err(|e| config_error_diagnostic(&e))?;

    let input = ResolveInput {
        explicit_path: global.cwd.clone(),
        explicit_name: global.vault.clone(),
        cwd,
        norn_root_env: std::env::var("NORN_ROOT").ok(),
    };

    let registry = Registry::new(home);
    let resolved = registry
        .resolve(&input)
        .map_err(|e| config_error_diagnostic(&e))?;

    // A registered vault may carry a `[vaults.<name>].config` override; the
    // summoned owner warms under it (ADR 0017 resolver-derived config). An
    // unregistered cwd (the common ephemeral case) has no override — the owner
    // loads `<root>/.norn/config.yaml`.
    let config_override = match &resolved.name {
        Some(name) => registry.lookup(name).ok().flatten().and_then(|v| v.config),
        None => None,
    };

    let exe = std::env::current_exe()
        .map_err(|e| Diagnostic::new(format!("cannot locate the norn executable: {e}")))?;
    let config = SummonConfig::for_vault(resolved.root, exe)
        .map_err(|e| client_error_diagnostic(&e))?
        .with_config_override(config_override);

    let mut session = open(&config).map_err(|e| client_error_diagnostic(&e))?;
    session
        .wait_until_ready(MAX_WAIT)
        .map_err(|e| client_error_diagnostic(&e))?;
    Ok(session)
}

/// Map a summoner [`ClientError`] onto a soft-landing [`Diagnostic`] (NRN-361).
///
/// The single CLI-side conversion for every read verb's `session.<verb>()`
/// failure AND every [`open_session`] summon failure — so the hint attached to
/// (say) an owner that went away is written once and rendered identically
/// everywhere. Hints are attached only where a genuinely useful next step
/// exists; a bare headline is a complete diagnostic otherwise.
pub fn client_error_diagnostic(e: &ClientError) -> Diagnostic {
    match e {
        // A config-resolution failure carries the richest diagnosis paths.
        ClientError::Resolve(cfg) => config_error_diagnostic(cfg),
        // The owner rejected a well-formed request (bad predicate, unresolvable
        // `--links-to`, a bad-config warm-up): the message is user-facing, and
        // any structured hints the owner attached ride straight through.
        ClientError::Rejected { message, hints } => {
            Diagnostic::new(message).with_hints(hints.iter().cloned())
        }
        // A self-healable transient that surfaced anyway (the owner exited at the
        // connection level after Ready): re-running summons a fresh owner.
        ClientError::OwnerGone(_) => Diagnostic::new(e.to_string())
            .with_hint("the vault owner exited before replying — re-run the command"),
        // Reachable but hung: a real diagnosis path exists via the service verbs.
        ClientError::OwnerHealth(_) => Diagnostic::new(e.to_string())
            .with_hint("the vault owner is not responding — check `norn service status`, or retry"),
        // The remaining variants (runtime-dir security, foreign owner, spawn,
        // protocol, raw IO) are self-explanatory headlines with no honest next
        // step to add — a hint here would be noise.
        _ => Diagnostic::new(e.to_string()),
    }
}

/// Map a central-config [`ConfigError`] onto a soft-landing [`Diagnostic`]
/// (NRN-361). The headline is the error's own message; a hint is added for the
/// variants with a concrete recovery path.
///
/// Public because the `vault` registry verbs fold their `ConfigError`s through
/// this same constructor (NRN-370), so a variant like `UnknownName` carries its
/// `norn vault list` hint identically whether it surfaced from a read summon or
/// from `vault set <unknown>`.
pub fn config_error_diagnostic(e: &ConfigError) -> Diagnostic {
    let base = Diagnostic::new(e.to_string());
    match e {
        ConfigError::UnknownName { .. } => {
            base.with_hint("run `norn vault list` to see registered vault names")
        }
        ConfigError::StaleEntry { .. } => base.with_hint(
            "the registered path no longer exists — re-register with `norn vault register`",
        ),
        ConfigError::BindingUnregistered { .. } => {
            base.with_hint("register the vault with `norn vault register`, or fix the binding file")
        }
        ConfigError::ConfigParse { .. } => base.with_hint(
            "fix the YAML syntax, then re-run — `norn config validate` reports the details",
        ),
        // `#[non_exhaustive]`: every other variant is a clear headline on its own.
        _ => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn unknown_vault_name_gets_a_vault_list_hint() {
        let e = ConfigError::UnknownName {
            name: "atlas".into(),
        };
        let diag = config_error_diagnostic(&e);
        assert!(diag.message().contains("atlas"));
        assert_eq!(
            diag.hints(),
            ["run `norn vault list` to see registered vault names".to_string()]
        );
    }

    #[test]
    fn stale_entry_gets_a_reregister_hint() {
        let e = ConfigError::StaleEntry {
            name: "atlas".into(),
            path: PathBuf::from("/gone"),
        };
        let diag = config_error_diagnostic(&e);
        assert_eq!(diag.hints().len(), 1);
        assert!(diag.hints()[0].contains("re-register"));
    }

    #[test]
    fn rejected_rides_wire_hints_through_verbatim() {
        let e = ClientError::Rejected {
            message: "bad predicate `type`".into(),
            hints: vec!["did you mean `--eq type:note`?".into()],
        };
        let diag = client_error_diagnostic(&e);
        assert_eq!(diag.message(), "bad predicate `type`");
        assert_eq!(diag.hints(), ["did you mean `--eq type:note`?".to_string()]);
    }

    #[test]
    fn owner_gone_gets_a_rerun_hint_but_raw_io_stays_bare() {
        let gone = client_error_diagnostic(&ClientError::OwnerGone("eof".into()));
        assert_eq!(gone.hints().len(), 1, "owner-gone earns a re-run hint");

        let io = client_error_diagnostic(&ClientError::Io(std::io::Error::other("boom")));
        assert!(io.hints().is_empty(), "a raw IO error adds no filler hint");
    }
}
